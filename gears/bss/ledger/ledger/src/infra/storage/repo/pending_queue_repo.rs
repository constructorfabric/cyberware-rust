//! `PendingQueueRepo` — the durable work-state Source-of-Truth (`SoT`) for the
//! deferred-apply queue (`bss.ledger_pending_event_queue`), keyed by
//! `(tenant_id, flow, business_id)`.
//!
//! ## Two-transaction `SoT` split
//! A deferred request rides on **two** durable rows written in the same intake
//! transaction, with distinct authoritative roles:
//! - **this** queue row holds the *work-state* `SoT` — the financial-key snapshot
//!   (`payload`) plus the lifecycle `status` (`QUEUED` → `APPLIED` | `CANCELLED`)
//!   and the retry `attempts` the applier drives. The applier reads, claims, and
//!   flips these rows; nothing else owns "is this work still to do".
//! - the **idempotency-dedup** row (written by [`IdempotencyGate::claim_queued`])
//!   holds the *replay reference* `SoT` — it answers "has this `(tenant, flow,
//!   business_id)` already been accepted?" for a duplicate intake, and later
//!   carries the posted `result_entry_id` once the apply finalizes. It is the
//!   at-most-once gate, not the work list.
//!
//! Keeping the two apart means a replayed intake is rejected by the dedup gate
//! without ever touching the queue, and the applier drains the queue without
//! re-deriving dedup state. (`IdempotencyGate`: [`crate::infra::posting::idempotency`].)
//!
//! ## In-txn vs out-of-txn
//! Writes (`insert_queued` at intake; `claim_due` / `mark_*` /
//! `bump_attempts_and_defer` in the applier) run inside a passed-in secure
//! transaction (`txn: &DbTx<'_>`)
//! — they mirror [`PaymentRepo`](super::PaymentRepo)'s in-txn counter writes
//! (scoped insert via `.secure().scope_with_model`; scoped `update_many` via
//! `.secure().scope_with`). The single read (`count_by_status`, a metric / sweep
//! depth) runs out-of-txn via `self.db.conn()`. Every query is scoped
//! (`.secure().scope_with…`) for SQL-level BOLA — a foreign tenant sees no rows.

use sea_orm::ExprTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveValue::Set,
    ColumnTrait, Condition, DbBackend, EntityTrait, Order, QuerySelect,
    sea_query::{LockBehavior, LockType},
};
use toolkit_db::secure::{AccessScope, DbTx, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::model::RepoError;
use crate::infra::posting::idempotency::STATUS_QUEUED;
use crate::infra::storage::entity::pending_event_queue;
use time::OffsetDateTime;

/// Status literal for a queue row whose deferred effect has been durably
/// applied (drained) in a later transaction — terminal, never re-claimed.
const STATUS_APPLIED: &str = "APPLIED";
/// Status literal for a queue row whose deferred effect was abandoned before
/// apply (e.g. quarantine give-up) — terminal, never re-claimed.
const STATUS_CANCELLED: &str = "CANCELLED";

/// One queue row to enqueue at intake. The lifecycle starts at `QUEUED` with
/// `attempts = 0`; `apply_after` is the earliest instant the applier may claim
/// the row (`None` = immediately eligible).
pub struct NewQueueRow {
    pub tenant_id: Uuid,
    pub flow: String,
    pub business_id: String,
    pub payload: serde_json::Value,
    pub queued_at: OffsetDateTime,
    pub apply_after: Option<OffsetDateTime>,
}

/// SeaORM-backed deferred-apply queue repository (work-state `SoT`). See the
/// module docs for the two-transaction `SoT` split against the dedup row.
#[derive(Clone)]
pub struct PendingQueueRepo {
    db: DBProvider<DbError>,
}

impl PendingQueueRepo {
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    // --- In-txn writes (intake enqueue + applier claim / lifecycle flips) ---

    /// Enqueue one queue row at intake: a scoped insert seeding `status = QUEUED`
    /// and `attempts = 0` (mirrors [`PaymentRepo::insert_allocation_rows`]). The
    /// PK `(tenant, flow, business_id)` makes a duplicate enqueue of the same
    /// business key collide — but the dedup gate's `claim_queued` runs first in
    /// the same intake txn and short-circuits a replay before reaching here, so
    /// an unexpected duplicate surfaces as [`RepoError::Db`].
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn insert_queued(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        row: &NewQueueRow,
    ) -> Result<(), RepoError> {
        let am = pending_event_queue::ActiveModel {
            tenant_id: Set(row.tenant_id),
            flow: Set(row.flow.clone()),
            business_id: Set(row.business_id.clone()),
            payload: Set(row.payload.clone()),
            queued_at: Set(row.queued_at),
            apply_after: Set(row.apply_after),
            status: Set(STATUS_QUEUED.to_owned()),
            attempts: Set(0),
        };
        pending_event_queue::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("pending_event_queue scope: {e}")))?
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("insert pending_event_queue: {e}")))?;
        Ok(())
    }

    /// Claim up to `limit` *due* `QUEUED` rows for `(tenant, flow)`, oldest
    /// `queued_at` first. "Due" means `apply_after IS NULL OR apply_after <=
    /// now`. The claimed rows are returned **still `QUEUED`** — claiming only
    /// reserves them under the row lock for this applier pass; the apply path
    /// flips each to `APPLIED` / `CANCELLED` (and re-gates it against the dedup,
    /// so this read is deliberately not payment-specific).
    ///
    /// ## SKIP LOCKED (Postgres only)
    /// On Postgres the select takes `FOR UPDATE SKIP LOCKED` so a concurrent
    /// applier skips the rows another pass is currently holding rather than
    /// blocking on them. This only REDUCES overlap — it does NOT hand each
    /// applier a disjoint batch: the row lock lives only for this short claim
    /// txn, and claiming does NOT flip the status (the `→APPLIED` flip rides the
    /// later apply txn), so the rows stay `QUEUED` and a second applier whose
    /// claim runs after this one commits can re-select the very same rows.
    /// Exactly-once is therefore enforced downstream by the apply's `SERIALIZABLE`
    /// post txn (dedup-row finalize + the `payment_allocation` PK), NOT by this
    /// lock. `SQLite` has no `FOR UPDATE`; the lock clause is omitted there (the
    /// queue applier is a Postgres-runtime feature — there are no concurrent
    /// appliers under `SQLite`, which the unit/test path uses). The backend is
    /// read from the provider (`self.db`), not the opaque `txn` handle, which
    /// exposes no backend accessor.
    ///
    /// Takes `&self` (unlike the other in-txn writes) precisely to reach the
    /// provider for that backend probe — mirroring [`JournalRepo`]'s in-txn
    /// `&self, txn` methods.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn claim_due(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        flow: &str,
        now: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<pending_event_queue::Model>, RepoError> {
        // Apply the row lock on the raw `find()` Select *before* wrapping it in
        // SecureORM: `SecureSelect` exposes `.filter/.order_by/.limit` but no
        // lock passthrough, and the lock clause is carried on the underlying
        // SelectStatement, so it survives `.secure().scope_with(...)`.
        let mut find = pending_event_queue::Entity::find();
        if self.db.db().backend() == DbBackend::Postgres {
            find = find.lock_with_behavior(LockType::Update, LockBehavior::SkipLocked);
        }
        let due = Condition::any()
            .add(pending_event_queue::Column::ApplyAfter.is_null())
            .add(pending_event_queue::Column::ApplyAfter.lte(now));
        let rows = find
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(pending_event_queue::Column::TenantId.eq(tenant))
                    .add(pending_event_queue::Column::Flow.eq(flow))
                    .add(pending_event_queue::Column::Status.eq(STATUS_QUEUED))
                    .add(due),
            )
            .order_by(pending_event_queue::Column::QueuedAt, Order::Asc)
            .limit(limit)
            .all(txn)
            .await
            .map_err(|e| RepoError::Db(format!("claim due pending_event_queue: {e}")))?;
        Ok(rows)
    }

    /// Flip one queue row to `APPLIED` (terminal) — the drain succeeded.
    /// Scoped `update_many` keyed on the full PK (mirrors the
    /// [`IdempotencyGate::finalize`] / [`PaymentRepo::add_allocated`] update
    /// shape). A zero-row update means the row vanished or was already
    /// terminal; surfaced as [`RepoError::Db`].
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope / storage failure or if no row matched.
    pub async fn mark_applied(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        flow: &str,
        business_id: &str,
    ) -> Result<(), RepoError> {
        set_status(txn, scope, tenant, flow, business_id, STATUS_APPLIED).await
    }

    /// Flip one queue row to `CANCELLED` (terminal) — the work was abandoned
    /// before apply (e.g. quarantine give-up). See [`Self::mark_applied`] for
    /// the update shape.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope / storage failure or if no row matched.
    pub async fn mark_cancelled(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        flow: &str,
        business_id: &str,
    ) -> Result<(), RepoError> {
        set_status(txn, scope, tenant, flow, business_id, STATUS_CANCELLED).await
    }

    /// Increment one queue row's `attempts` (`attempts = attempts + 1`) AND defer
    /// its next eligibility to `apply_after` (the backoff instant) — recording a
    /// failed/retried apply pass that must not be re-claimed until then. Setting
    /// `apply_after` is what keeps `claim_due` / `list_all_due` (both gate on
    /// `apply_after IS NULL OR apply_after <= now`) from re-selecting a durably
    /// `Blocked` row on every pass. Scoped `update_many` with a `col + 1`
    /// expression (mirrors [`PaymentRepo::add_allocated`]'s version bump). A
    /// zero-row update surfaces as [`RepoError::Db`].
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope / storage failure or if no row matched.
    pub async fn bump_attempts_and_defer(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        flow: &str,
        business_id: &str,
        apply_after: OffsetDateTime,
    ) -> Result<(), RepoError> {
        let result = pending_event_queue::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                pending_event_queue::Column::Attempts,
                Expr::col((
                    pending_event_queue::Entity,
                    pending_event_queue::Column::Attempts,
                ))
                .add(1),
            )
            .col_expr(
                pending_event_queue::Column::ApplyAfter,
                Expr::value(Some(apply_after)),
            )
            .filter(
                Condition::all()
                    .add(pending_event_queue::Column::TenantId.eq(tenant))
                    .add(pending_event_queue::Column::Flow.eq(flow))
                    .add(pending_event_queue::Column::BusinessId.eq(business_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("bump pending_event_queue attempts: {e}")))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "pending_event_queue row absent for ({tenant}, {flow}, {business_id})"
            )));
        }
        Ok(())
    }

    // --- Out-of-txn cross-tenant read (system-context sweep; UNSCOPED) ---

    /// List up to `limit` *due* `QUEUED` rows for `flow` ACROSS ALL TENANTS,
    /// oldest `queued_at` first — the system-context feed for the periodic sweep
    /// job ([`crate::infra::jobs::queue_applier`]). "Due" means `apply_after IS
    /// NULL OR apply_after <= now`, exactly as [`Self::claim_due`].
    ///
    /// DELIBERATELY UNSCOPED (`AccessScope::allow_all()`, no `.secure()` tenant
    /// narrowing) — this is a cross-tenant admin/system read, the same sanctioned
    /// pattern as [`crate::infra::storage::repo::ReferenceRepo::list_all_fiscal_calendars`]
    /// (the period-open sweep). The caller (the sweep job) re-narrows to one
    /// tenant via `AccessScope::for_tenant(row.tenant_id)` before applying each
    /// row, so every WRITE remains tenant-scoped; only this read is broad. Runs
    /// out-of-txn via `self.db.conn()` (the apply then claims under SKIP LOCKED in
    /// its own txn, so this read is just the candidate feed — a row that another
    /// applier grabs first is simply skipped at claim time).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage failure.
    pub async fn list_all_due(
        &self,
        flow: &str,
        now: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<pending_event_queue::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let due = Condition::any()
            .add(pending_event_queue::Column::ApplyAfter.is_null())
            .add(pending_event_queue::Column::ApplyAfter.lte(now));
        let rows = pending_event_queue::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(
                Condition::all()
                    .add(pending_event_queue::Column::Flow.eq(flow))
                    .add(pending_event_queue::Column::Status.eq(STATUS_QUEUED))
                    .add(due),
            )
            .order_by(pending_event_queue::Column::QueuedAt, Order::Asc)
            .limit(limit)
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list all due pending_event_queue: {e}")))?;
        Ok(rows)
    }

    // --- Out-of-txn read (metric / sweep depth; SQL-level BOLA) ---

    /// Read the single queue row for `(tenant, flow, business_id)`, or `None`
    /// when absent. The out-of-txn counterpart to the intake `insert_queued`:
    /// the allocate early-dedup check (`AllocationService::allocate_inner`) calls
    /// this on a `QUEUED` dedup-status replay to surface the prior row's
    /// `queued_at` for the `Queued` handle (the dedup row carries no
    /// `queued_at`; the queue row is its work-state `SoT`). Like the other reads
    /// it runs via `self.db.conn()` and is scoped (`.secure().scope_with`) for
    /// SQL-level BOLA — a foreign tenant reads `None`.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn get(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        flow: &str,
        business_id: &str,
    ) -> Result<Option<pending_event_queue::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let row = pending_event_queue::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(pending_event_queue::Column::TenantId.eq(tenant))
                    .add(pending_event_queue::Column::Flow.eq(flow))
                    .add(pending_event_queue::Column::BusinessId.eq(business_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("get pending_event_queue: {e}")))?;
        Ok(row)
    }

    /// Count the `flow` rows currently in `status` ACROSS ALL TENANTS — the
    /// system-context queue-depth feed for the sweep job's
    /// `ledger_allocation_queue_depth` gauge. DELIBERATELY UNSCOPED
    /// (`AccessScope::allow_all()`), the same sanctioned cross-tenant pattern as
    /// [`Self::list_all_due`]; the gauge is an operational backlog metric, not a
    /// tenant-visible read. Returned as `i64` for the metric sink.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a storage failure.
    pub async fn count_all_by_status(&self, flow: &str, status: &str) -> Result<i64, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let count = pending_event_queue::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(
                Condition::all()
                    .add(pending_event_queue::Column::Flow.eq(flow))
                    .add(pending_event_queue::Column::Status.eq(status)),
            )
            .count(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("count all pending_event_queue by status: {e}")))?;
        Ok(i64::try_from(count).unwrap_or(i64::MAX))
    }

    /// Count the `(tenant, flow)` rows currently in `status` (e.g. queue depth
    /// for a `QUEUED` gauge, or drained/cancelled totals). SQL-level BOLA: a
    /// foreign tenant counts zero. `SeaORM`'s count is `u64`; it is returned as
    /// `i64` for the metric sink — a queue depth never approaches `i64::MAX`.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn count_by_status(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        flow: &str,
        status: &str,
    ) -> Result<i64, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let count = pending_event_queue::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(pending_event_queue::Column::TenantId.eq(tenant))
                    .add(pending_event_queue::Column::Flow.eq(flow))
                    .add(pending_event_queue::Column::Status.eq(status)),
            )
            .count(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("count pending_event_queue by status: {e}")))?;
        Ok(i64::try_from(count).unwrap_or(i64::MAX))
    }
}

/// Scoped `update_many` flipping one queue row's `status` to `new_status`, keyed
/// on the full PK. Shared by `mark_applied` / `mark_cancelled`. A zero-row
/// update means the row vanished or was already terminal — surfaced as
/// [`RepoError::Db`].
async fn set_status(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant: Uuid,
    flow: &str,
    business_id: &str,
    new_status: &str,
) -> Result<(), RepoError> {
    let result = pending_event_queue::Entity::update_many()
        .secure()
        .scope_with(scope)
        .col_expr(
            pending_event_queue::Column::Status,
            Expr::value(new_status.to_owned()),
        )
        .filter(
            Condition::all()
                .add(pending_event_queue::Column::TenantId.eq(tenant))
                .add(pending_event_queue::Column::Flow.eq(flow))
                .add(pending_event_queue::Column::BusinessId.eq(business_id)),
        )
        .exec(txn)
        .await
        .map_err(|e| RepoError::Db(format!("set pending_event_queue status {new_status}: {e}")))?;
    if result.rows_affected == 0 {
        return Err(RepoError::Db(format!(
            "pending_event_queue row absent for ({tenant}, {flow}, {business_id})"
        )));
    }
    Ok(())
}
