//! `ApprovalRepo` — dual-control approval state (`bss.ledger_approval`) plus the
//! append-only comment thread (`bss.ledger_approval_comment`).
//!
//! The pending-create, the decision transitions (approve / reject /
//! request-changes / cancel / expire), and the resubmit run **inside the caller's
//! transaction** (the approval row locks in the pre-balance slot just before
//! `fiscal_period`, §4.3). State transitions use an **optimistic guard on the
//! expected current state** (mirroring [`DisputeRepo::dispute_advance`]): a
//! transition matched against the wrong state touches 0 rows, so a concurrent
//! decision (e.g. two approvers) leaves exactly one winner — the loser maps the
//! `0` to an invalid transition. Reads (queue + single + thread) run out-of-txn
//! through the PDP-compiled `AccessScope` (SQL-level BOLA — a foreign tenant
//! yields no row). Idempotency (DC13) is the partial-unique index on
//! `(tenant, kind, business_key) WHERE state IN ('PENDING','NEEDS_REWORK')`; the
//! service reads the active record before inserting.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, Order};
use serde_json::Value as JsonValue;
use toolkit_db::secure::{AccessScope, DbTx, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::approval::ApprovalState;
use crate::domain::approval::policy::{DualControlPolicy, PolicyVersion};
use crate::domain::error::DomainError;
use crate::domain::model::RepoError;
use crate::infra::storage::entity::{
    dual_control_approval as approval, dual_control_comment as comment,
    dual_control_policy as policy,
};
use time::OffsetDateTime;

/// Owned seed for a fresh `PENDING` approval row (preparer step).
#[derive(Clone)]
pub struct NewPendingApproval {
    pub approval_id: Uuid,
    pub tenant: Uuid,
    pub kind: String,
    pub business_key: String,
    pub intent: JsonValue,
    pub amount_usd_eq_minor: Option<i64>,
    pub threshold_snapshot: JsonValue,
    pub reason_code: String,
    pub prepared_by: Uuid,
    pub prepared_at: OffsetDateTime,
    pub correlation_id: Uuid,
    pub expires_at: OffsetDateTime,
}

/// Owned seed for a fresh effective-dated dual-control policy version (DC8). The
/// `version` is computed by the caller as `max(version) + 1` inside the same
/// serializable txn as the insert.
#[derive(Clone)]
pub struct NewPolicyVersion {
    pub tenant: Uuid,
    pub version: i64,
    pub effective_from: OffsetDateTime,
    pub d2_threshold_minor: i64,
    pub a6_backdating_biz_days: i32,
    pub pending_ttl_seconds: i64,
    pub created_at_utc: OffsetDateTime,
}

/// SeaORM-backed dual-control approval repository.
#[derive(Clone)]
pub struct ApprovalRepo {
    db: DBProvider<DbError>,
}

impl ApprovalRepo {
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    // --- In-txn writes (called by the ApprovalService) ---

    /// Insert a fresh `PENDING` row (`revision = 0`). The caller has already
    /// confirmed no active record exists for `(tenant, kind, business_key)`
    /// (DC13); a racing duplicate hits the partial-unique index and errors.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure (incl. the active-row
    /// uniqueness violation).
    pub async fn insert_pending(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        row: NewPendingApproval,
    ) -> Result<(), RepoError> {
        let am = approval::ActiveModel {
            approval_id: Set(row.approval_id),
            tenant_id: Set(row.tenant),
            kind: Set(row.kind),
            state: Set(ApprovalState::Pending.as_str().to_owned()),
            revision: Set(0),
            business_key: Set(row.business_key),
            intent: Set(row.intent),
            amount_usd_eq_minor: Set(row.amount_usd_eq_minor),
            threshold_snapshot: Set(row.threshold_snapshot),
            reason_code: Set(row.reason_code),
            prepared_by: Set(row.prepared_by),
            prepared_at: Set(row.prepared_at),
            approved_by: Set(None),
            decided_at: Set(None),
            correlation_id: Set(row.correlation_id),
            expires_at: Set(row.expires_at),
        };
        approval::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("ledger_approval scope: {e}")))?
            .exec_with_returning(txn)
            .await
            .map_err(|e| RepoError::Db(format!("insert ledger_approval: {e}")))?;
        Ok(())
    }

    /// Read the approval row inside the decision transaction (the service checks
    /// `state` + `prepared_by` before executing the stored `intent`).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_in_txn(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        approval_id: Uuid,
    ) -> Result<Option<approval::Model>, RepoError> {
        approval::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(approval::Column::TenantId.eq(tenant))
                    .add(approval::Column::ApprovalId.eq(approval_id)),
            )
            .one(txn)
            .await
            .map_err(|e| RepoError::Db(format!("read ledger_approval: {e}")))
    }

    /// Transition the row from `expected_state` to `new_state`, stamping the
    /// decider + decision time. Matched on `(tenant, approval_id, state =
    /// expected_state)` — the in-txn optimistic backstop. Returns the rows
    /// affected: `0` means the row was not in `expected_state` (a concurrent
    /// decision won, or a stale request), which the caller maps to an invalid
    /// transition.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    #[allow(clippy::too_many_arguments)] // a decision write is intrinsically wide
    pub async fn transition(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        approval_id: Uuid,
        expected_state: &str,
        new_state: &str,
        decider: Option<Uuid>,
        decided_at: Option<OffsetDateTime>,
    ) -> Result<u64, RepoError> {
        let result = approval::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(approval::Column::State, Expr::value(new_state))
            .col_expr(approval::Column::ApprovedBy, Expr::value(decider))
            .col_expr(approval::Column::DecidedAt, Expr::value(decided_at))
            .filter(
                Condition::all()
                    .add(approval::Column::TenantId.eq(tenant))
                    .add(approval::Column::ApprovalId.eq(approval_id))
                    .add(approval::Column::State.eq(expected_state)),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("transition ledger_approval: {e}")))?;
        Ok(result.rows_affected)
    }

    /// Resubmit a `NEEDS_REWORK` row back to `PENDING` with the preparer's edited
    /// intent + re-snapshot threshold, bumping `revision`. Matched on `(tenant,
    /// approval_id, state = NEEDS_REWORK)`; `0` rows = not awaiting rework.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    #[allow(clippy::too_many_arguments)] // a resubmit re-snapshots several columns
    pub async fn resubmit(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        approval_id: Uuid,
        new_intent: JsonValue,
        new_threshold_snapshot: JsonValue,
        new_amount_usd_eq_minor: Option<i64>,
        new_revision: i32,
    ) -> Result<u64, RepoError> {
        let result = approval::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                approval::Column::State,
                Expr::value(ApprovalState::Pending.as_str()),
            )
            .col_expr(approval::Column::Intent, Expr::value(new_intent))
            .col_expr(
                approval::Column::ThresholdSnapshot,
                Expr::value(new_threshold_snapshot),
            )
            .col_expr(
                approval::Column::AmountUsdEqMinor,
                Expr::value(new_amount_usd_eq_minor),
            )
            .col_expr(approval::Column::Revision, Expr::value(new_revision))
            .filter(
                Condition::all()
                    .add(approval::Column::TenantId.eq(tenant))
                    .add(approval::Column::ApprovalId.eq(approval_id))
                    .add(approval::Column::State.eq(ApprovalState::NeedsRework.as_str())),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("resubmit ledger_approval: {e}")))?;
        Ok(result.rows_affected)
    }

    /// Batch-expire active rows past `expires_at` (sweep job; also runnable as a
    /// lazy pass). Returns the number expired.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn expire_due(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, RepoError> {
        let result = approval::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                approval::Column::State,
                Expr::value(ApprovalState::Expired.as_str()),
            )
            .filter(
                Condition::all()
                    .add(approval::Column::TenantId.eq(tenant))
                    .add(
                        approval::Column::State
                            .is_in(ApprovalState::ACTIVE.map(ApprovalState::as_str)),
                    )
                    // `<= now` (not `< now`): a row whose `expires_at` lands exactly
                    // on `now` is reclaimable in this same pass, closing the
                    // one-instant dead zone vs `read_active`'s `ExpiresAt > now`.
                    .add(approval::Column::ExpiresAt.lte(now)),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("expire ledger_approval: {e}")))?;
        Ok(result.rows_affected)
    }

    /// Append a comment to the thread (a free comment/question, or the mandatory
    /// reason on a `reject` / `request-changes` decision). Append-only — there is
    /// no update/delete path.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    #[allow(clippy::too_many_arguments)] // a flat append row; a struct adds churn
    pub async fn append_comment(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        comment_id: Uuid,
        approval_id: Uuid,
        tenant: Uuid,
        revision: i32,
        author_actor: Uuid,
        body: String,
        created_at: OffsetDateTime,
    ) -> Result<(), RepoError> {
        let am = comment::ActiveModel {
            comment_id: Set(comment_id),
            approval_id: Set(approval_id),
            tenant_id: Set(tenant),
            revision: Set(revision),
            author_actor: Set(author_actor),
            body: Set(body),
            created_at: Set(created_at),
        };
        comment::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("ledger_approval_comment scope: {e}")))?
            .exec_with_returning(txn)
            .await
            .map_err(|e| RepoError::Db(format!("insert ledger_approval_comment: {e}")))?;
        Ok(())
    }

    /// Cross-tenant TTL sweep (DC12): flip every active (`PENDING`/`NEEDS_REWORK`)
    /// approval whose `expires_at` has passed to `EXPIRED`, across all tenants, in
    /// one statement — the system-context reaper pattern
    /// ([`AccessScope::allow_all`], like the tie-out / aged-alarm sweeps; expiry is
    /// platform maintenance, not a tenant-scoped action). `APPROVING` is excluded
    /// (an in-flight approve is not expirable). Complements the lazy per-tenant
    /// [`Self::expire_due`] pass in `create_pending`; returns the number expired.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a scope or storage failure.
    pub async fn expire_due_all(&self, now: OffsetDateTime) -> Result<u64, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        let result = approval::Entity::update_many()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .col_expr(
                approval::Column::State,
                Expr::value(ApprovalState::Expired.as_str()),
            )
            .filter(
                Condition::all()
                    .add(
                        approval::Column::State
                            .is_in(ApprovalState::ACTIVE.map(ApprovalState::as_str)),
                    )
                    .add(approval::Column::ExpiresAt.lte(now)),
            )
            .exec(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("expire_due_all ledger_approval: {e}")))?;
        Ok(result.rows_affected)
    }

    /// Count `ledger_approval` rows currently in the transient `APPROVING` latch,
    /// across all tenants (Z8-1). A healthy approve clears the latch within one txn
    /// (latch → execute → mark), so a non-zero result observed by the maintenance
    /// sweep is a crash-stranded approve — excluded from the TTL sweep
    /// ([`Self::expire_due_all`]) and still holding the active-uniqueness slot — that
    /// needs a manual re-approve. System-context reaper read
    /// ([`AccessScope::allow_all`], like the TTL sweep).
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a scope or storage failure.
    pub async fn count_approving_all(&self) -> Result<u64, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        approval::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            // Tie the query to the domain enum's wire token (the single source of
            // truth, mirrored by the migration CHECK) rather than a bare literal.
            .filter(
                Condition::all().add(approval::Column::State.eq(ApprovalState::Approving.as_str())),
            )
            .count(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("count_approving_all ledger_approval: {e}")))
    }

    /// The greatest existing policy `version` for `tenant`, or `None` when the
    /// tenant has no policy row yet — the seed for the next version number
    /// (`max + 1`), read inside the same serializable txn as the insert so two
    /// concurrent writers cannot mint the same version (the PK `(tenant, version)`
    /// is the backstop).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn max_policy_version(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
    ) -> Result<Option<i64>, RepoError> {
        let row = policy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(policy::Column::TenantId.eq(tenant)))
            .order_by(policy::Column::Version, Order::Desc)
            .one(txn)
            .await
            .map_err(|e| RepoError::Db(format!("max dual_control_policy version: {e}")))?;
        Ok(row.map(|r| r.version))
    }

    /// Insert one effective-dated dual-control policy version (DC8). Append-only —
    /// a new threshold set is a new `(tenant, version)` row, never an update; the
    /// resolver picks the latest `effective_from` (highest `version` on a tie). The
    /// D2/A6/TTL CHECK ranges are the DB backstop; the caller validates first
    /// (`validate_config` → `DualControlPolicyOutOfRange`).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure (incl. a `(tenant, version)`
    /// PK collision from a concurrent writer).
    pub async fn insert_policy_row(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        row: NewPolicyVersion,
    ) -> Result<(), RepoError> {
        let am = policy::ActiveModel {
            tenant_id: Set(row.tenant),
            version: Set(row.version),
            effective_from: Set(row.effective_from),
            d2_threshold_minor: Set(row.d2_threshold_minor),
            a6_backdating_biz_days: Set(row.a6_backdating_biz_days),
            pending_ttl_seconds: Set(row.pending_ttl_seconds),
            created_at_utc: Set(row.created_at_utc),
        };
        policy::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("ledger_dual_control_policy scope: {e}")))?
            .exec_with_returning(txn)
            .await
            .map_err(|e| RepoError::Db(format!("insert ledger_dual_control_policy: {e}")))?;
        Ok(())
    }

    // --- Out-of-txn reads (PDP In-scoped; SQL-level BOLA) ---

    /// Read a single approval for `(tenant, approval_id)`, or `None`. A foreign
    /// tenant yields no row.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a scope or storage failure.
    pub async fn read(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        approval_id: Uuid,
    ) -> Result<Option<approval::Model>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        approval::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(approval::Column::TenantId.eq(tenant))
                    .add(approval::Column::ApprovalId.eq(approval_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("read ledger_approval: {e}")))
    }

    /// Read all effective-dated dual-control policy versions for a tenant (the
    /// threshold resolver picks the one in effect). Empty ⇒ ratified defaults.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a scope or storage failure.
    pub async fn read_policy_versions(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
    ) -> Result<Vec<PolicyVersion>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        let rows = policy::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(policy::Column::TenantId.eq(tenant)))
            .all(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("read dual_control_policy: {e}")))?;
        Ok(rows
            .into_iter()
            .map(|r| PolicyVersion {
                effective_from: r.effective_from,
                version: r.version,
                policy: DualControlPolicy {
                    d2_threshold_minor: r.d2_threshold_minor,
                    a6_backdating_biz_days: r.a6_backdating_biz_days,
                    pending_ttl_seconds: r.pending_ttl_seconds,
                },
            })
            .collect())
    }

    /// Read the single active (`PENDING`/`NEEDS_REWORK`) approval for
    /// `(tenant, kind, business_key)`, if any — the idempotency lookup (DC13) the
    /// service does before creating a fresh pending record. The partial-unique
    /// index guarantees at most one.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a scope or storage failure.
    pub async fn read_active(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        kind: &str,
        business_key: &str,
        now: OffsetDateTime,
    ) -> Result<Option<approval::Model>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        approval::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(approval::Column::TenantId.eq(tenant))
                    .add(approval::Column::Kind.eq(kind))
                    .add(approval::Column::BusinessKey.eq(business_key))
                    // `APPROVING` (the H2 execute latch) is also active — an
                    // idempotent re-prepare while an approve is mid-flight returns
                    // the in-flight record rather than colliding on the partial
                    // unique with no row to surface.
                    .add(approval::Column::State.is_in([
                        ApprovalState::Pending.as_str(),
                        ApprovalState::NeedsRework.as_str(),
                        ApprovalState::Approving.as_str(),
                    ]))
                    // A lapsed approval (past its TTL) is no longer active: it must
                    // not win the idempotent short-circuit, and a fresh prepare must
                    // be able to replace it (the lazy `expire_due` pass in
                    // `create_pending` flips it to EXPIRED inside the insert txn).
                    .add(approval::Column::ExpiresAt.gt(now)),
            )
            .one(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("read_active ledger_approval: {e}")))
    }

    /// List the approval queue for a tenant, optionally filtered by `state` and
    /// `kind`. Newest-first by `prepared_at` (sorted in memory).
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a scope or storage failure.
    pub async fn list(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        state: Option<&str>,
        kind: Option<&str>,
    ) -> Result<Vec<approval::Model>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        let mut predicate = Condition::all().add(approval::Column::TenantId.eq(tenant));
        if let Some(s) = state {
            predicate = predicate.add(approval::Column::State.eq(s));
        }
        if let Some(k) = kind {
            predicate = predicate.add(approval::Column::Kind.eq(k));
        }
        let mut rows = approval::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(predicate)
            .all(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("list ledger_approval: {e}")))?;
        rows.sort_by_key(|r| std::cmp::Reverse(r.prepared_at));
        Ok(rows)
    }

    /// Read the full comment thread for an approval, oldest-first.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a scope or storage failure.
    pub async fn read_thread(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        approval_id: Uuid,
    ) -> Result<Vec<comment::Model>, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("conn: {e}")))?;
        let mut rows = comment::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(comment::Column::TenantId.eq(tenant))
                    .add(comment::Column::ApprovalId.eq(approval_id)),
            )
            .all(&conn)
            .await
            .map_err(|e| DomainError::Internal(format!("read thread: {e}")))?;
        rows.sort_by_key(|r| r.created_at);
        Ok(rows)
    }
}
