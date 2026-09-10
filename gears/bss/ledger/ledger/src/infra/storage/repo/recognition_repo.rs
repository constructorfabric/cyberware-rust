//! `RecognitionRepo` — the ASC 606 revenue-recognition tables
//! (`recognition_schedule`, `recognition_segment`, `recognition_run`), keyed by
//! `(tenant_id, schedule_id)` / `(tenant_id, schedule_id, segment_no)` /
//! `(tenant_id, run_id)`.
//!
//! The **writes** (`insert_schedule`, `insert_segments`, `add_recognized`) run
//! inside the passed-in posting transaction (the in-txn sidecar, decision M),
//! mirroring [`PaymentRepo`](super::PaymentRepo)'s `seed_settlement` /
//! `insert_allocation_rows` / `add_*` shape: a scoped insert via
//! `.secure().scope_with_model`, a scoped `update_many` via
//! `.secure().scope_with`. `insert_schedule` materializes a fresh ACTIVE
//! schedule in the same transaction as the Slice 1 Contract-liability credit
//! (design §4.2 — a deferred balance never exists without a schedule);
//! `add_recognized` is the counter-delta-under-lock the `RecognitionRunner`
//! (Phase 2) applies per released segment. The `recognized_minor <=
//! total_deferred_minor` cap CHECK is the authoritative per-obligation
//! over-recognition guard under `SERIALIZABLE` (design §4.3 / §7); a violation
//! surfaces as [`RepoError::MoneyOutCapExceeded`] (the runner's stamp sidecar
//! turns it into the `OVER_RECOGNITION` wire code), exactly as
//! `PaymentRepo::add_*` maps its per-payment cap CHECKs.
//!
//! The **reads** (`read_schedule`, `list_segments`) take the PDP-compiled
//! `AccessScope` and run out-of-txn through `.secure().scope_with(scope)`
//! (SQL-level BOLA — a foreign tenant yields no rows); segments are ordered by
//! `segment_no` (which is 1:1 with `period_id`, so this is also period order).

use std::collections::HashMap;

use bss_ledger_sdk::{AccountClass, Side, SourceDocType};

use sea_orm::ExprTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, DbErr, EntityTrait, Order};
use toolkit_db::secure::{
    AccessScope, DbTx, ScopeError, SecureEntityExt, SecureInsertExt, SecureUpdateExt,
};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::model::RepoError;
use crate::domain::status::{
    PERIOD_STATUS_OPEN, RUN_STATUS_DONE, RUN_STATUS_FAILED, RUN_STATUS_RUNNING,
    SCHEDULE_STATUS_ACTIVE, SCHEDULE_STATUS_COMPLETED, SEGMENT_STATUS_DONE, SEGMENT_STATUS_PENDING,
    SEGMENT_STATUS_QUEUED,
};
use toolkit_db::odata::sea_orm_filter::{LimitCfg, paginate_odata};
use toolkit_odata::{ODataQuery, Page, SortDir};

use crate::infra::storage::entity::{
    fiscal_period, journal_entry, journal_line, recognition_run, recognition_schedule,
    recognition_segment,
};
use crate::infra::storage::odata_mapping::RecognitionRunODataMapper;
use crate::infra::storage::repo::journal_repo::{
    OdataPageError, map_odata_err, query_with_default_order,
};
use crate::odata::RecognitionRunFilterField;
use time::OffsetDateTime;

/// The `recognition_schedule` row to insert for a freshly materialized ACTIVE
/// schedule (one per revenue stream, design §3.5 / §4.5). `recognized_minor`
/// starts at 0 and `version` at 0 (set by the repo); `status` is stamped
/// `ACTIVE`.
pub struct NewSchedule {
    pub tenant_id: Uuid,
    pub schedule_id: String,
    pub payer_tenant_id: Uuid,
    pub source_invoice_id: String,
    pub source_invoice_item_ref: String,
    pub po_allocation_group: Option<String>,
    pub subscription_ref: Option<String>,
    pub revenue_stream: String,
    pub currency: String,
    pub total_deferred_minor: i64,
    pub policy_ref: String,
    pub ssp_snapshot_ref: Option<String>,
    pub vc_estimate_ref: Option<String>,
    pub vc_method_ref: Option<String>,
}

/// One `recognition_segment` row to insert — a time- or milestone-slice of a
/// schedule. `segment_no` is immutable and 1:1 with `period_id`; rows are seeded
/// `PENDING` with `recognized_at`/`run_id` NULL (stamped on release in Phase 2).
pub struct NewSegment {
    pub tenant_id: Uuid,
    pub schedule_id: String,
    pub segment_no: i32,
    pub period_id: String,
    pub amount_minor: i64,
}

/// A new ACTIVE **replacement** schedule version minted by a `replace` change
/// (Group H, design §3.6): the successor of a now-`REPLACED` schedule. Carries
/// the SAME business-key dims as its predecessor (so the partial UNIQUE one-live
/// guard still holds — the old flips `REPLACED` in the SAME txn before this
/// inserts), an explicit `version = old.version + 1`, and the REMAINING deferred
/// (`old.total_deferred − old.recognized`) as its `total_deferred_minor`. Mirrors
/// [`NewSchedule`] but with the explicit lineage `version` (the build path always
/// seeds `version = 0`).
pub struct ReplacementSchedule {
    pub tenant_id: Uuid,
    pub schedule_id: String,
    pub payer_tenant_id: Uuid,
    pub source_invoice_id: String,
    pub source_invoice_item_ref: String,
    pub po_allocation_group: Option<String>,
    pub subscription_ref: Option<String>,
    pub revenue_stream: String,
    pub currency: String,
    pub total_deferred_minor: i64,
    pub policy_ref: String,
    pub ssp_snapshot_ref: Option<String>,
    pub vc_estimate_ref: Option<String>,
    pub vc_method_ref: Option<String>,
    /// The lineage version of the successor (`= old.version + 1`).
    pub version: i64,
}

/// A due `PENDING` segment paired with the stream/currency/account context of
/// its owning ACTIVE schedule — the unit the `RecognitionRunner` releases. The
/// join to `recognition_schedule` carries the `revenue_stream` + `currency`
/// (both legs of the `DR CONTRACT_LIABILITY / CR REVENUE` post need them) and
/// the `total_deferred_minor`/`recognized_minor` snapshot (read-only context;
/// the authoritative over-recognition guard is the in-txn cap CHECK, not this
/// snapshot — design §4.3). A foreign tenant yields no rows (SQL-level BOLA).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuePendingSegment {
    pub schedule_id: String,
    pub segment_no: i32,
    pub period_id: String,
    pub amount_minor: i64,
    pub revenue_stream: String,
    pub currency: String,
    pub total_deferred_minor: i64,
    pub recognized_minor: i64,
}

/// One disaggregated recognized-revenue grain (design §3.5 / §4.5): the **net**
/// revenue RECOGNIZED into `revenue_stream` during `period_id` — the *actual*
/// posting period (see below) — in minor units of `currency`. The
/// [`RecognitionRepo::list_revenue_disaggregation`] read sources this from the
/// **journal** (not the segment rows): the `REVENUE` lines of the tenant's
/// `RECOGNITION` entries (each release posts `DR CONTRACT_LIABILITY / CR
/// REVENUE`; each clawback the mirror `DR REVENUE / CR CONTRACT_LIABILITY`),
/// grouped by `(period_id, revenue_stream)` and **signed-summed** (a `CR` release
/// adds, a `DR` reversal subtracts), ordered by `(period_id, revenue_stream)`. A
/// foreign tenant yields no rows (SQL-level BOLA).
///
/// **Why the journal, not the segment's `period_id`.** A segment keeps its
/// *planned* `period_id` as the audit target even when an E-2 missed-close
/// releases it into the current OPEN period; the journal entry (and so its lines)
/// carries that actual open period. Sourcing from the entry's REVENUE lines
/// therefore reports the period the revenue truly landed in, and nets out
/// reversals — both of which a DONE-segment scan (gross, planned-period) cannot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecognizedStreamEntry {
    pub period_id: String,
    pub revenue_stream: String,
    pub recognized_minor: i64,
    pub currency: String,
}

/// SeaORM-backed recognition schedule/segment/run repository.
#[derive(Clone)]
pub struct RecognitionRepo {
    db: DBProvider<DbError>,
}

impl RecognitionRepo {
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    // --- In-txn writes (called by the schedule-build / recognition sidecars) ---

    /// Insert the `recognition_schedule` row for a freshly materialized ACTIVE
    /// schedule (`recognized_minor = 0`, `version = 0`, `status = ACTIVE`). The
    /// partial `UNIQUE (tenant, source_invoice_id, source_invoice_item_ref,
    /// revenue_stream) WHERE status='ACTIVE'` is the at-most-one-live guard — a
    /// concurrent second live schedule for the same business key collides; but a
    /// duplicate build is short-circuited by the `SCHEDULE_BUILD` idempotency
    /// claim before the sidecar, so an unexpected collision surfaces as
    /// [`RepoError::Db`].
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn insert_schedule(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        schedule: &NewSchedule,
    ) -> Result<(), RepoError> {
        let am = recognition_schedule::ActiveModel {
            tenant_id: Set(schedule.tenant_id),
            schedule_id: Set(schedule.schedule_id.clone()),
            payer_tenant_id: Set(schedule.payer_tenant_id),
            source_invoice_id: Set(schedule.source_invoice_id.clone()),
            source_invoice_item_ref: Set(schedule.source_invoice_item_ref.clone()),
            po_allocation_group: Set(schedule.po_allocation_group.clone()),
            subscription_ref: Set(schedule.subscription_ref.clone()),
            revenue_stream: Set(schedule.revenue_stream.clone()),
            currency: Set(schedule.currency.clone()),
            total_deferred_minor: Set(schedule.total_deferred_minor),
            recognized_minor: Set(0),
            policy_ref: Set(schedule.policy_ref.clone()),
            ssp_snapshot_ref: Set(schedule.ssp_snapshot_ref.clone()),
            vc_estimate_ref: Set(schedule.vc_estimate_ref.clone()),
            vc_method_ref: Set(schedule.vc_method_ref.clone()),
            status: Set(SCHEDULE_STATUS_ACTIVE.to_owned()),
            version: Set(0),
        };
        recognition_schedule::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("recognition_schedule scope: {e}")))?
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("insert recognition_schedule: {e}")))?;
        Ok(())
    }

    /// Insert the N `recognition_segment` rows for one schedule (each seeded
    /// `PENDING`, `recognized_at`/`run_id` NULL). The PK
    /// `(tenant, schedule_id, segment_no)` and the period UNIQUE make a replay of
    /// the same schedule collide — but a duplicate build returns before the
    /// sidecar (the `SCHEDULE_BUILD` claim), so this is only reached on the first
    /// build; an unexpected duplicate surfaces as a storage [`DbError`].
    ///
    /// Returns [`DbError`] (NOT [`RepoError`]): this is an in-txn write driven by
    /// both the build sidecar and the Group H change txn, and the change txn
    /// retries on a serialization conflict — so the inner `sea_orm::DbErr` is
    /// PRESERVED (`DbError::Sea`) for the retry helper's `as_db_err`, mirroring
    /// [`crate::infra::period_close`]'s `scope_to_db`. A scope-construction fault
    /// (never a serialization conflict) stays a non-retryable `DbError::Other`.
    ///
    /// # Errors
    /// [`DbError`] on a scope or storage failure.
    pub async fn insert_segments(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        segments: &[NewSegment],
    ) -> Result<(), DbError> {
        for seg in segments {
            let am = recognition_segment::ActiveModel {
                tenant_id: Set(seg.tenant_id),
                schedule_id: Set(seg.schedule_id.clone()),
                segment_no: Set(seg.segment_no),
                period_id: Set(seg.period_id.clone()),
                amount_minor: Set(seg.amount_minor),
                status: Set(SEGMENT_STATUS_PENDING.to_owned()),
                recognized_at: Set(None),
                run_id: Set(None),
            };
            recognition_segment::Entity::insert(am.clone())
                .secure()
                .scope_with_model(scope, &am)
                .map_err(scope_to_db)?
                .exec(txn)
                .await
                .map_err(scope_to_db)?;
        }
        Ok(())
    }

    /// In-txn scoped read of the `recognition_schedule` row for
    /// `(tenant, schedule_id)`, or `None` when absent — the read-half a Group H
    /// schedule change runs INSIDE its serializable change transaction (so the
    /// read, the status flip, and any replacement insert share one snapshot and
    /// conflict with a concurrent release under SSI). The out-of-txn
    /// [`Self::read_schedule`] is the read surface for REST `GET`s; this is its
    /// in-txn twin. SQL-level BOLA: a foreign tenant yields no row.
    ///
    /// Returns [`DbError`] (NOT [`RepoError`]): driven by the Group H change txn,
    /// which retries on a serialization conflict — so the inner `sea_orm::DbErr`
    /// (incl. a `40001` raised reading the contended schedule row mid-statement)
    /// is PRESERVED as `DbError::Sea` for the retry helper, mirroring
    /// [`crate::infra::period_close`]'s `scope_to_db`.
    ///
    /// # Errors
    /// [`DbError`] on a scope or storage failure.
    pub async fn read_schedule_in_txn(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
    ) -> Result<Option<recognition_schedule::Model>, DbError> {
        let row = recognition_schedule::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(recognition_schedule::Column::TenantId.eq(tenant))
                    .add(recognition_schedule::Column::ScheduleId.eq(schedule_id)),
            )
            .one(txn)
            .await
            .map_err(scope_to_db)?;
        Ok(row)
    }

    /// In-txn scoped read of the ACTIVE successor of a `REPLACED` schedule (the
    /// Group H replay path): the one ACTIVE schedule sharing the predecessor's
    /// business key (`source_invoice_id`, `source_invoice_item_ref`,
    /// `revenue_stream`) at the successor lineage `version` (`= old.version + 1`).
    /// A `replace` mints exactly one such row, so this resolves the new
    /// `schedule_id` an idempotent change replay reports. `None` when no such
    /// ACTIVE successor exists (e.g. the successor was itself later replaced — a
    /// degenerate replay window). Runs INSIDE the change txn (the claim guard
    /// already holds the task-local conn-bypass guard, so an out-of-txn `conn()`
    /// would fail). SQL-level BOLA: a foreign tenant yields no row.
    ///
    /// Returns [`DbError`] (NOT [`RepoError`]) so a serialization conflict raised
    /// mid-statement in the change txn stays retryable (`DbError::Sea`), mirroring
    /// [`crate::infra::period_close`]'s `scope_to_db`.
    ///
    /// # Errors
    /// [`DbError`] on a scope or storage failure.
    pub async fn read_active_successor_in_txn(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        source_invoice_id: &str,
        source_invoice_item_ref: &str,
        revenue_stream: &str,
        version: i64,
    ) -> Result<Option<recognition_schedule::Model>, DbError> {
        let row = recognition_schedule::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(recognition_schedule::Column::TenantId.eq(tenant))
                    .add(recognition_schedule::Column::SourceInvoiceId.eq(source_invoice_id))
                    .add(
                        recognition_schedule::Column::SourceInvoiceItemRef
                            .eq(source_invoice_item_ref),
                    )
                    .add(recognition_schedule::Column::RevenueStream.eq(revenue_stream))
                    .add(recognition_schedule::Column::Version.eq(version))
                    .add(recognition_schedule::Column::Status.eq(SCHEDULE_STATUS_ACTIVE)),
            )
            .one(txn)
            .await
            .map_err(scope_to_db)?;
        Ok(row)
    }

    /// In-txn scoped read of the LIVE (`ACTIVE`) schedule for a business key
    /// `(tenant, source_invoice_id, source_invoice_item_ref, revenue_stream)`,
    /// regardless of `version` — the at-most-one-live partial UNIQUE guarantees 0
    /// or 1 row. A later deferring note (a debit note) reads it here to EXTEND it
    /// (one ACTIVE schedule per key) rather than mint a second the partial UNIQUE
    /// would reject. SQL-level BOLA: a foreign tenant yields no row.
    ///
    /// # Errors
    /// [`DbError`] on a scope or storage failure.
    pub async fn read_active_schedule_in_txn(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        source_invoice_id: &str,
        source_invoice_item_ref: &str,
        revenue_stream: &str,
    ) -> Result<Option<recognition_schedule::Model>, DbError> {
        let row = recognition_schedule::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(recognition_schedule::Column::TenantId.eq(tenant))
                    .add(recognition_schedule::Column::SourceInvoiceId.eq(source_invoice_id))
                    .add(
                        recognition_schedule::Column::SourceInvoiceItemRef
                            .eq(source_invoice_item_ref),
                    )
                    .add(recognition_schedule::Column::RevenueStream.eq(revenue_stream))
                    .add(recognition_schedule::Column::Status.eq(SCHEDULE_STATUS_ACTIVE)),
            )
            .one(txn)
            .await
            .map_err(scope_to_db)?;
        Ok(row)
    }

    /// Transition a schedule's `status` from `from_status` to `to_status` (e.g.
    /// `ACTIVE → REPLACED` / `ACTIVE → CANCELLED`), bumping `version`, for
    /// `(tenant, schedule_id)` — the Group H mark step (design §3.6 / §4.6). The
    /// filter requires the current status to be `from_status`, so the flip is the
    /// idempotency/race backstop: a concurrent change (or a replay that already
    /// transitioned the row) matches no row (`rows_affected == 0`), which the
    /// caller treats as "already transitioned" rather than an error. A scoped
    /// `update_many` inside the change txn. Returns the number of rows flipped
    /// (`0` or `1`).
    ///
    /// Returns [`DbError`] (NOT [`RepoError`]): the change txn retries on a
    /// serialization conflict, and the `ACTIVE` row this flips is exactly what a
    /// concurrent release contends — so a `40001` raised here is PRESERVED as
    /// `DbError::Sea` for the retry helper, mirroring
    /// [`crate::infra::period_close`]'s `scope_to_db` (a non-retryable
    /// scope-construction fault stays `DbError::Other`).
    ///
    /// # Errors
    /// [`DbError`] on a scope or storage failure.
    pub async fn mark_schedule_status(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
        from_status: &str,
        to_status: &str,
    ) -> Result<u64, DbError> {
        let result = recognition_schedule::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                recognition_schedule::Column::Status,
                Expr::value(to_status.to_owned()),
            )
            .col_expr(
                recognition_schedule::Column::Version,
                Expr::col((
                    recognition_schedule::Entity,
                    recognition_schedule::Column::Version,
                ))
                .add(1),
            )
            .filter(
                Condition::all()
                    .add(recognition_schedule::Column::TenantId.eq(tenant))
                    .add(recognition_schedule::Column::ScheduleId.eq(schedule_id))
                    .add(recognition_schedule::Column::Status.eq(from_status)),
            )
            .exec(txn)
            .await
            .map_err(scope_to_db)?;
        Ok(result.rows_affected)
    }

    /// In-txn scoped read of the highest `period_id` among a schedule's
    /// already-`DONE` segments, or `None` when none are `DONE` — the floor a
    /// Group H `replace` validates its replacement periods against (design §4.6):
    /// a replacement segment may never re-target a period the old schedule has
    /// ALREADY recognized (that would re-recognize a closed period across the
    /// version boundary — cross-version double-recognition), so the first
    /// replacement period MUST be strictly greater than this. `period_id` is the
    /// `YYYYMM` lexical-sortable string, so `ORDER BY period_id DESC LIMIT 1` is
    /// the max-DONE-period read (1:1 with `segment_no` within a schedule). Runs
    /// INSIDE the change txn (the claim guard already holds the task-local
    /// conn-bypass guard, so an out-of-txn `conn()` would fail), so it joins the
    /// serializable snapshot. SQL-level BOLA: a foreign tenant yields no row.
    ///
    /// Returns [`DbError`] (NOT [`RepoError`]) so a serialization conflict raised
    /// mid-statement in the change txn stays retryable (`DbError::Sea`), mirroring
    /// the other in-txn change helpers.
    ///
    /// # Errors
    /// [`DbError`] on a scope or storage failure.
    pub async fn max_done_segment_period_in_txn(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
    ) -> Result<Option<String>, DbError> {
        let row = recognition_segment::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(recognition_segment::Column::TenantId.eq(tenant))
                    .add(recognition_segment::Column::ScheduleId.eq(schedule_id))
                    .add(recognition_segment::Column::Status.eq(SEGMENT_STATUS_DONE)),
            )
            .order_by(recognition_segment::Column::PeriodId, Order::Desc)
            .one(txn)
            .await
            .map_err(scope_to_db)?;
        Ok(row.map(|r| r.period_id))
    }

    /// Count `recognition_segment`s whose `period_id` is `<=` the closing period
    /// and not yet `DONE` — the period-close gate input (a due segment that has
    /// not released blocks close, design §4.5). In-txn so it joins the close's
    /// `SERIALIZABLE` snapshot (a concurrent release conflicts under SSI).
    /// Returns [`DbError`] so a serialization conflict stays retryable.
    ///
    /// # Errors
    /// Returns [`DbError`] if scope application or the segment query fails.
    pub async fn count_due_not_done_in_txn(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
    ) -> Result<usize, DbError> {
        let rows = recognition_segment::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(recognition_segment::Column::TenantId.eq(tenant))
                    .add(recognition_segment::Column::PeriodId.lte(period_id))
                    .add(recognition_segment::Column::Status.ne(SEGMENT_STATUS_DONE)),
            )
            .all(txn)
            .await
            .map_err(scope_to_db)?;
        Ok(rows.len())
    }

    /// Insert a fresh ACTIVE **replacement** schedule version (Group H `replace`,
    /// design §3.6): `recognized_minor = 0`, `status = ACTIVE`, and the explicit
    /// lineage `version` the caller computed (`= old.version + 1`). Mirrors
    /// [`Self::insert_schedule`] but threads the explicit `version` (the build
    /// path always seeds `0`). The old schedule must already be flipped `REPLACED`
    /// in the SAME txn before this runs, so the partial `UNIQUE (tenant,
    /// source_invoice_id, source_invoice_item_ref, revenue_stream) WHERE
    /// status='ACTIVE'` one-live guard holds; an unexpected collision surfaces as
    /// a storage [`DbError`] and rolls the change back.
    ///
    /// Returns [`DbError`] (NOT [`RepoError`]) so a serialization conflict raised
    /// mid-statement in the change txn stays retryable (`DbError::Sea`), mirroring
    /// [`crate::infra::period_close`]'s `scope_to_db`.
    ///
    /// # Errors
    /// [`DbError`] on a scope or storage failure.
    pub async fn insert_replacement_schedule(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        schedule: &ReplacementSchedule,
    ) -> Result<(), DbError> {
        let am = recognition_schedule::ActiveModel {
            tenant_id: Set(schedule.tenant_id),
            schedule_id: Set(schedule.schedule_id.clone()),
            payer_tenant_id: Set(schedule.payer_tenant_id),
            source_invoice_id: Set(schedule.source_invoice_id.clone()),
            source_invoice_item_ref: Set(schedule.source_invoice_item_ref.clone()),
            po_allocation_group: Set(schedule.po_allocation_group.clone()),
            subscription_ref: Set(schedule.subscription_ref.clone()),
            revenue_stream: Set(schedule.revenue_stream.clone()),
            currency: Set(schedule.currency.clone()),
            total_deferred_minor: Set(schedule.total_deferred_minor),
            recognized_minor: Set(0),
            policy_ref: Set(schedule.policy_ref.clone()),
            ssp_snapshot_ref: Set(schedule.ssp_snapshot_ref.clone()),
            vc_estimate_ref: Set(schedule.vc_estimate_ref.clone()),
            vc_method_ref: Set(schedule.vc_method_ref.clone()),
            status: Set(SCHEDULE_STATUS_ACTIVE.to_owned()),
            version: Set(schedule.version),
        };
        recognition_schedule::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(scope_to_db)?
            .exec(txn)
            .await
            .map_err(scope_to_db)?;
        Ok(())
    }

    /// Increment `recognition_schedule.recognized_minor` by `amount` for a
    /// released segment, bumping `version`. The `recognized_minor <=
    /// total_deferred_minor` cap CHECK
    /// (`chk_ledger_recognition_schedule_recognized_le_deferred`) is the
    /// authoritative per-obligation over-recognition guard: it enforces that the
    /// cumulative release never exceeds what was deferred, evaluated against the
    /// resulting row (`recognized_minor + amount <= total_deferred_minor`). A
    /// violation maps to [`RepoError::MoneyOutCapExceeded`] (the recognition
    /// stamp sidecar refines it to the `OVER_RECOGNITION` 409). A scoped UPDATE,
    /// not an upsert: the schedule row always pre-exists (the schedule is
    /// materialized before any release), so an `INSERT … ON CONFLICT` would trip
    /// the CHECK on the INSERT VALUES tuple during arbitration (see
    /// `PaymentRepo::add_allocated`). SSI + retry serialize concurrent releases
    /// of the same schedule; `rows_affected == 0` ⇒ no such schedule.
    ///
    /// # Errors
    /// [`RepoError::MoneyOutCapExceeded`] when the cap CHECK rejects the
    /// increment; [`RepoError::Db`] when no row matched or on any other scope /
    /// storage failure.
    pub async fn add_recognized(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
        amount: i64,
    ) -> Result<(), RepoError> {
        // Scoped UPDATE (not an upsert), exactly like `PaymentRepo::add_*`: the
        // CHECK evaluates against the resulting row (`recognized_minor + amount <=
        // total_deferred_minor`), so an over-recognition surfaces as the CHECK
        // violation, mapped to `MoneyOutCapExceeded`; the runner's stamp sidecar
        // refines it to `OverRecognition` (409).
        let result = recognition_schedule::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                recognition_schedule::Column::RecognizedMinor,
                Expr::col((
                    recognition_schedule::Entity,
                    recognition_schedule::Column::RecognizedMinor,
                ))
                .add(amount),
            )
            .col_expr(
                recognition_schedule::Column::Version,
                Expr::col((
                    recognition_schedule::Entity,
                    recognition_schedule::Column::Version,
                ))
                .add(1),
            )
            .filter(
                Condition::all()
                    .add(recognition_schedule::Column::TenantId.eq(tenant))
                    .add(recognition_schedule::Column::ScheduleId.eq(schedule_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| map_cap_violation("add recognized_minor", &e))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "recognition_schedule row absent for ({tenant}, {schedule_id})"
            )));
        }
        Ok(())
    }

    /// Decrement `recognition_schedule.total_deferred_minor` by `amount` (a
    /// **positive** reduction over the not-yet-released remainder), bumping
    /// `version` — the Slice-3 **credit-note deferred reduction** (design §4.2):
    /// when a credit note debits `CONTRACT_LIABILITY` for a deferred portion it
    /// reduces the owning schedule's deferred total in the SAME post txn, so a
    /// later recognition run cannot re-recognize the credited-back amount. The
    /// reduction is bounded by the schedule's remaining releasable amount
    /// (`total_deferred_minor − recognized_minor`): the authoritative guard is the
    /// existing `recognized_minor <= total_deferred_minor` CHECK
    /// (`chk_ledger_recognition_schedule_recognized_le_deferred`) — it is evaluated
    /// against the resulting row, so a reduction that would drop
    /// `total_deferred_minor` below the already-`recognized_minor` (over-reducing an
    /// in-flight schedule) is rejected; the `total_deferred_minor >= 0` CHECK is the
    /// floor. A violation maps to [`RepoError::MoneyOutCapExceeded`] (the
    /// `CreditNoteHandler` refines it — already-released segments are never
    /// recomputed, mirroring Slice 4 §4.6 re-version semantics).
    ///
    /// A scoped UPDATE (not an upsert), exactly like [`Self::add_recognized`]: the
    /// schedule row always pre-exists (a deferred portion implies an ACTIVE
    /// schedule the split read). SSI + retry serialize a concurrent credit-note
    /// reduction and a recognition release of the same schedule (both take the
    /// rank-6 schedule row). `rows_affected == 0` ⇒ no such schedule (an invariant
    /// breach — the split read it under the lock order).
    ///
    /// # Errors
    /// [`RepoError::MoneyOutCapExceeded`] when a schedule CHECK rejects the
    /// reduction (over-reduction past the releasable remainder, or below zero);
    /// [`RepoError::Db`] when no row matched or on any other scope / storage
    /// failure.
    pub async fn reduce_deferred(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
        amount: i64,
    ) -> Result<(), RepoError> {
        // Apply as a NEGATIVE delta on `total_deferred_minor` (`col + (−amount)`),
        // the SAME `col_expr(col, Expr::col(col).add(delta))` shape `add_recognized`
        // uses (the reversal path likewise feeds it a negative delta) — so the CHECK
        // is evaluated against the resulting row exactly as for the recognized
        // counter, and the SQL is the proven counter-delta-under-lock.
        let neg_delta = amount.checked_neg().ok_or_else(|| {
            RepoError::Db(format!(
                "deferred reduction amount {amount} overflows on negate"
            ))
        })?;
        let result = recognition_schedule::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                recognition_schedule::Column::TotalDeferredMinor,
                Expr::col((
                    recognition_schedule::Entity,
                    recognition_schedule::Column::TotalDeferredMinor,
                ))
                .add(neg_delta),
            )
            .col_expr(
                recognition_schedule::Column::Version,
                Expr::col((
                    recognition_schedule::Entity,
                    recognition_schedule::Column::Version,
                ))
                .add(1),
            )
            .filter(
                Condition::all()
                    .add(recognition_schedule::Column::TenantId.eq(tenant))
                    .add(recognition_schedule::Column::ScheduleId.eq(schedule_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| map_cap_violation("reduce total_deferred_minor", &e))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "recognition_schedule row absent for ({tenant}, {schedule_id})"
            )));
        }
        Ok(())
    }

    /// Increase `total_deferred_minor` by `amount` (a positive delta) + bump
    /// `version`, for `(tenant, schedule_id)` — a later deferring note (a debit
    /// note) ADDS its deferred part to the live schedule it extends. The same
    /// `col_expr(col, col + delta)` shape as [`Self::reduce_deferred`] (its
    /// inverse), so the `deferred >= 0` CHECK is evaluated against the resulting
    /// row; the `recognized <= total_deferred` CHECK can only relax (the total
    /// grows). `rows_affected == 0` ⇒ no such schedule.
    ///
    /// # Errors
    /// [`RepoError::Db`] when no row matched or on a scope / storage failure.
    pub async fn increase_total_deferred(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
        amount: i64,
    ) -> Result<(), RepoError> {
        let result = recognition_schedule::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                recognition_schedule::Column::TotalDeferredMinor,
                Expr::col((
                    recognition_schedule::Entity,
                    recognition_schedule::Column::TotalDeferredMinor,
                ))
                .add(amount),
            )
            .col_expr(
                recognition_schedule::Column::Version,
                Expr::col((
                    recognition_schedule::Entity,
                    recognition_schedule::Column::Version,
                ))
                .add(1),
            )
            .filter(
                Condition::all()
                    .add(recognition_schedule::Column::TenantId.eq(tenant))
                    .add(recognition_schedule::Column::ScheduleId.eq(schedule_id)),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("increase total_deferred_minor: {e}")))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "recognition_schedule row absent for ({tenant}, {schedule_id})"
            )));
        }
        Ok(())
    }

    /// In-txn scoped read of all segments for `(tenant, schedule_id)`, ascending by
    /// `segment_no` — the read-half of a schedule EXTEND (a debit note merging its
    /// segments into the live schedule needs the current period set + the max
    /// `segment_no` within the SAME txn). The in-txn twin of [`Self::list_segments`].
    ///
    /// # Errors
    /// [`DbError`] on a scope or storage failure.
    pub async fn list_segments_in_txn(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
    ) -> Result<Vec<recognition_segment::Model>, DbError> {
        let rows = recognition_segment::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(recognition_segment::Column::TenantId.eq(tenant))
                    .add(recognition_segment::Column::ScheduleId.eq(schedule_id)),
            )
            .order_by(recognition_segment::Column::SegmentNo, Order::Asc)
            .all(txn)
            .await
            .map_err(scope_to_db)?;
        Ok(rows)
    }

    /// Add `amount` (a positive delta) to an EXISTING `PENDING` segment's
    /// `amount_minor`, for `(tenant, schedule_id, segment_no)` — a debit note
    /// extending the live schedule on a period it already covers folds its amount
    /// into that period's segment (one row per period, UNIQUE
    /// (`schedule_id`, `period_id`)). The filter requires `status = 'PENDING'` so a
    /// period already released (`DONE`) or parked (`QUEUED`) is NOT silently grown
    /// (that would strand revenue the run already recognized); the caller treats
    /// `rows_affected == 0` as "cannot extend that period" and rolls the post back.
    ///
    /// # Errors
    /// [`RepoError::Db`] when no PENDING row matched or on a scope / storage failure.
    pub async fn add_pending_segment_amount(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
        segment_no: i32,
        amount: i64,
    ) -> Result<(), RepoError> {
        let result = recognition_segment::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                recognition_segment::Column::AmountMinor,
                Expr::col((
                    recognition_segment::Entity,
                    recognition_segment::Column::AmountMinor,
                ))
                .add(amount),
            )
            .filter(
                Condition::all()
                    .add(recognition_segment::Column::TenantId.eq(tenant))
                    .add(recognition_segment::Column::ScheduleId.eq(schedule_id))
                    .add(recognition_segment::Column::SegmentNo.eq(segment_no))
                    .add(recognition_segment::Column::Status.eq(SEGMENT_STATUS_PENDING)),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("add segment amount: {e}")))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "no PENDING recognition_segment for ({tenant}, {schedule_id}, seg \
                 {segment_no}) — cannot extend an already-released/parked period"
            )));
        }
        Ok(())
    }

    /// Transition a fully-drained schedule `ACTIVE → COMPLETED` (design §4.6),
    /// for `(tenant, schedule_id)` — the terminal stamp the recognition stamp
    /// sidecar applies on the RELEASE path after the last segment commits `DONE`.
    /// The filter requires the current `status` to be `ACTIVE` AND
    /// `recognized_minor == total_deferred_minor` (a column-to-column equality:
    /// the schedule has recognized everything it deferred), so the flip fires
    /// exactly once — on the release that drains the last segment — and is a
    /// no-op (`rows_affected == 0`) on every earlier release (the schedule is not
    /// yet drained) and on a replay (already `COMPLETED`). Calling it after every
    /// `stamp_segment_done` is therefore correct + idempotent.
    ///
    /// **No `version` bump** (unlike [`Self::mark_schedule_status`] /
    /// [`Self::add_recognized`]): `COMPLETED` is the SAME schedule reaching its
    /// terminal state, not a new lineage (a `replace`/`cancel` mints a new
    /// version; completion does not). A scoped `update_many` inside the release
    /// post txn. Returns `true` iff the row flipped (the last segment just
    /// drained it), `false` otherwise (not yet drained, or already terminal).
    ///
    /// Reaching `COMPLETED` frees the partial `UNIQUE (tenant,
    /// source_invoice_id, source_invoice_item_ref, revenue_stream) WHERE
    /// status='ACTIVE'` one-live slot (a fresh deferred re-build of the same
    /// business key is then admitted) and drops the schedule from the runner's
    /// ACTIVE-only due feed + the `ledger_schedule_active_total` gauge.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn complete_schedule_if_drained(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
    ) -> Result<bool, RepoError> {
        let result = recognition_schedule::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                recognition_schedule::Column::Status,
                Expr::value(SCHEDULE_STATUS_COMPLETED.to_owned()),
            )
            .filter(
                Condition::all()
                    .add(recognition_schedule::Column::TenantId.eq(tenant))
                    .add(recognition_schedule::Column::ScheduleId.eq(schedule_id))
                    .add(recognition_schedule::Column::Status.eq(SCHEDULE_STATUS_ACTIVE))
                    .add(
                        Expr::col(recognition_schedule::Column::RecognizedMinor)
                            .eq(Expr::col(recognition_schedule::Column::TotalDeferredMinor)),
                    ),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("complete recognition_schedule: {e}")))?;
        Ok(result.rows_affected > 0)
    }

    /// Stamp one `recognition_segment` `DONE` (set `status = DONE`,
    /// `recognized_at`, `run_id`) for `(tenant, schedule_id, segment_no)` — the
    /// release marker the `RecognitionRunner`'s stamp sidecar writes in the SAME
    /// post txn as the `DR CL / CR Revenue` entry (design §4.3). The filter
    /// requires the current `status` to be `PENDING` or `QUEUED`, so a re-stamp of
    /// an already-`DONE` segment matches no row (`rows_affected == 0`): this is the
    /// at-most-once stamp guard, layered under the per-segment `RECOGNITION`
    /// idempotency claim (a replay returns before the sidecar) and the
    /// `UNIQUE (schedule, period_id)` key. `rows_affected == 0` therefore signals
    /// either a foreign/absent segment or a concurrent release that already flipped
    /// it — an invariant breach on the fresh-claim path, surfaced as
    /// [`RepoError::Db`] so the post rolls back rather than double-crediting.
    ///
    /// # Errors
    /// [`RepoError::Db`] when no `PENDING`/`QUEUED` row matched, or on any scope /
    /// storage failure.
    pub async fn stamp_segment_done(
        txn: &DbTx<'_>,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
        segment_no: i32,
        run_id: Uuid,
        recognized_at: OffsetDateTime,
    ) -> Result<(), RepoError> {
        let result = recognition_segment::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                recognition_segment::Column::Status,
                Expr::value(SEGMENT_STATUS_DONE.to_owned()),
            )
            .col_expr(
                recognition_segment::Column::RecognizedAt,
                Expr::value(Some(recognized_at)),
            )
            .col_expr(
                recognition_segment::Column::RunId,
                Expr::value(Some(run_id)),
            )
            .filter(
                Condition::all()
                    .add(recognition_segment::Column::TenantId.eq(tenant))
                    .add(recognition_segment::Column::ScheduleId.eq(schedule_id))
                    .add(recognition_segment::Column::SegmentNo.eq(segment_no))
                    .add(
                        recognition_segment::Column::Status
                            .is_in([SEGMENT_STATUS_PENDING, SEGMENT_STATUS_QUEUED]),
                    ),
            )
            .exec(txn)
            .await
            .map_err(|e| RepoError::Db(format!("stamp recognition_segment DONE: {e}")))?;
        if result.rows_affected == 0 {
            return Err(RepoError::Db(format!(
                "recognition_segment ({tenant}, {schedule_id}, {segment_no}) absent or not \
                 PENDING/QUEUED at stamp time"
            )));
        }
        Ok(())
    }

    // --- Out-of-txn reads (PDP In-scoped; SQL-level BOLA) ---

    /// Read the `recognition_schedule` row for `(tenant, schedule_id)`, or `None`
    /// when no such schedule exists. SQL-level BOLA: a foreign tenant yields no
    /// row.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_schedule(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
    ) -> Result<Option<recognition_schedule::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let row = recognition_schedule::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(recognition_schedule::Column::TenantId.eq(tenant))
                    .add(recognition_schedule::Column::ScheduleId.eq(schedule_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read recognition_schedule: {e}")))?;
        Ok(row)
    }

    /// List `recognition_schedule` headers for `tenant`, optionally narrowed to
    /// one originating invoice (`source_invoice_id`) and/or one `revenue_stream`.
    /// Backs the `GET /recognition-schedules` discovery surface and the
    /// post-commit lookup that surfaces a freshly-minted `schedule_id` on
    /// invoice-post. SQL-level BOLA: a foreign tenant yields no rows.
    ///
    /// Ordered by `(source_invoice_item_ref, version desc, schedule_id)` — the
    /// trailing PK makes it a TOTAL order so the cap truncates deterministically
    /// (schedules can share `(item_ref, version)`: different streams, archived
    /// lineage). Returns `(rows, truncated)`: `truncated` is `true` when the
    /// `(tenant[, stream])` scan exceeded the cap, so the caller can signal it
    /// rather than silently drop the tail.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn list_schedules(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        invoice_id: Option<&str>,
        revenue_stream: Option<&str>,
    ) -> Result<(Vec<recognition_schedule::Model>, bool), RepoError> {
        // A discovery lookup, not a paginated collection: a per-(tenant, invoice)
        // result is tiny; the cap only fences a `revenue_stream`-only scan. Fetch
        // one extra row to DETECT truncation (vs silently dropping the tail).
        const SCHEDULE_LIST_CAP: usize = 500;
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let mut predicate = Condition::all().add(recognition_schedule::Column::TenantId.eq(tenant));
        if let Some(invoice_id) = invoice_id {
            predicate = predicate.add(recognition_schedule::Column::SourceInvoiceId.eq(invoice_id));
        }
        if let Some(revenue_stream) = revenue_stream {
            predicate =
                predicate.add(recognition_schedule::Column::RevenueStream.eq(revenue_stream));
        }
        let mut rows = recognition_schedule::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(predicate)
            .order_by(
                recognition_schedule::Column::SourceInvoiceItemRef,
                Order::Asc,
            )
            .order_by(recognition_schedule::Column::Version, Order::Desc)
            .order_by(recognition_schedule::Column::ScheduleId, Order::Asc)
            .limit(SCHEDULE_LIST_CAP as u64 + 1)
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list recognition_schedule: {e}")))?;
        let truncated = rows.len() > SCHEDULE_LIST_CAP;
        if truncated {
            rows.truncate(SCHEDULE_LIST_CAP);
            tracing::warn!(
                tenant = %tenant,
                cap = SCHEDULE_LIST_CAP,
                "recognition-schedule list hit the cap; result truncated (no pagination)"
            );
        }
        Ok((rows, truncated))
    }

    /// List the `recognition_segment` rows for `(tenant, schedule_id)`, ordered
    /// by `segment_no` (1:1 with `period_id`, so this is also period order).
    /// SQL-level BOLA: a foreign tenant yields no rows.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn list_segments(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
    ) -> Result<Vec<recognition_segment::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let rows = recognition_segment::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(recognition_segment::Column::TenantId.eq(tenant))
                    .add(recognition_segment::Column::ScheduleId.eq(schedule_id)),
            )
            .order_by(recognition_segment::Column::SegmentNo, Order::Asc)
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list recognition_segment: {e}")))?;
        Ok(rows)
    }

    /// List the due releasable segments for `(tenant, period_id ≤ target)`,
    /// joined to their ACTIVE schedule for the stream/currency/account context the
    /// `RecognitionRunner` posts with, ordered by `schedule_id` then `segment_no`
    /// (the runner releases each schedule's due segments in ascending `segment_no`
    /// — the ordering GAP guard that the predecessor be `DONE` is Group E). A
    /// segment whose schedule is not ACTIVE (`COMPLETED`/`REPLACED`/`CANCELLED`) is
    /// dropped — a `COMPLETED` schedule has no due work and a `CANCELLED`/`REPLACED`
    /// one is not released under its old id (design §4.6). SQL-level BOLA: a
    /// foreign tenant yields no rows (both queries are `.secure().scope_with`).
    ///
    /// Implemented as a scoped segment scan + a per-distinct-schedule scoped
    /// lookup (the gear has no cross-entity join idiom; every repo read is a
    /// single-entity scoped query) — the schedule reads are bounded by the count
    /// of distinct due schedules and memoized here.
    ///
    /// **Releasable-from set (`PENDING` + `QUEUED`).** Both `PENDING` and a
    /// previously out-of-order-parked `QUEUED` segment are returned — the SAME
    /// acceptance set [`Self::stamp_segment_done`] flips to `DONE`. This is the
    /// Group F drain: a `QUEUED` segment is re-enumerated each run and releases
    /// once its lower-period predecessor has committed `DONE` (the runner's
    /// `count_predecessors_not_done` gate re-evaluates it); a still-blocked
    /// `QUEUED` segment is simply re-parked (a no-op `mark_segment_queued`). Only
    /// `DONE` segments are excluded (terminal — already released).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn list_due_pending_segments(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
    ) -> Result<Vec<DuePendingSegment>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;

        // Due releasable segments (PENDING or QUEUED, period_id ≤ target), ordered
        // by schedule then segment_no. `period_id` is the `YYYYMM`
        // lexical-sortable string, so a `<=` string compare is the period-order
        // compare (1:1 with segment_no within a schedule). A QUEUED segment is
        // re-enumerated so a later run drains it once its predecessor is DONE
        // (Group F); only DONE is excluded (terminal).
        let segments = recognition_segment::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(recognition_segment::Column::TenantId.eq(tenant))
                    .add(
                        recognition_segment::Column::Status
                            .is_in([SEGMENT_STATUS_PENDING, SEGMENT_STATUS_QUEUED]),
                    )
                    .add(recognition_segment::Column::PeriodId.lte(period_id)),
            )
            .order_by(recognition_segment::Column::ScheduleId, Order::Asc)
            .order_by(recognition_segment::Column::SegmentNo, Order::Asc)
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list due recognition_segment: {e}")))?;

        // Resolve each distinct owning schedule once (scoped), keeping only ACTIVE
        // ones; cache the join context per schedule_id.
        let mut schedule_ctx: HashMap<String, Option<recognition_schedule::Model>> = HashMap::new();
        let mut due = Vec::with_capacity(segments.len());
        for seg in segments {
            if !schedule_ctx.contains_key(&seg.schedule_id) {
                let row = recognition_schedule::Entity::find()
                    .secure()
                    .scope_with(scope)
                    .filter(
                        Condition::all()
                            .add(recognition_schedule::Column::TenantId.eq(tenant))
                            .add(recognition_schedule::Column::ScheduleId.eq(&seg.schedule_id)),
                    )
                    .one(&conn)
                    .await
                    .map_err(|e| {
                        RepoError::Db(format!("read recognition_schedule for due segment: {e}"))
                    })?;
                schedule_ctx.insert(seg.schedule_id.clone(), row);
            }
            // Only release segments of an ACTIVE schedule.
            let Some(schedule) = schedule_ctx
                .get(&seg.schedule_id)
                .and_then(Option::as_ref)
                .filter(|s| s.status == SCHEDULE_STATUS_ACTIVE)
            else {
                continue;
            };
            due.push(DuePendingSegment {
                schedule_id: seg.schedule_id,
                segment_no: seg.segment_no,
                period_id: seg.period_id,
                amount_minor: seg.amount_minor,
                revenue_stream: schedule.revenue_stream.clone(),
                currency: schedule.currency.clone(),
                total_deferred_minor: schedule.total_deferred_minor,
                recognized_minor: schedule.recognized_minor,
            });
        }
        Ok(due)
    }

    /// Disaggregate **net** RECOGNIZED revenue by stream for `(tenant, period_id?)`
    /// (design §3.5 / §4.5), sourced from the **journal** — the `REVENUE` lines of
    /// the tenant's `RECOGNITION` entries. Each segment release posts a
    /// `DR CONTRACT_LIABILITY / CR REVENUE` entry; each clawback the mirror
    /// `DR REVENUE / CR CONTRACT_LIABILITY`. The read groups those REVENUE lines by
    /// `(period_id, revenue_stream)` and **signed-sums** them (a `CR` release adds,
    /// a `DR` reversal subtracts → reversal-aware), ordered by
    /// `(period_id, revenue_stream)`. `period_id` `None` ⇒ every period; `Some(_)`
    /// narrows to that period. SQL-level BOLA: a foreign tenant yields no rows
    /// (every query is `.secure().scope_with`).
    ///
    /// **Period = the journal entry's period, not the segment's.** A segment keeps
    /// its *planned* `period_id` as the audit target even when an E-2 missed-close
    /// releases it into the current OPEN period (§4.3); the journal entry — and so
    /// each of its lines (every `journal_line.period_id` is persisted from the
    /// owning entry's period) — carries the actual open period. Reading the entry's
    /// REVENUE lines therefore reports the period the revenue truly landed in (and
    /// nets out reversals), which a DONE-segment scan (gross, planned-period)
    /// cannot.
    ///
    /// Implemented as two scoped single-entity reads + an in-memory group/SUM (the
    /// gear has no cross-entity join / DB-side `GROUP BY` idiom): (1) the tenant's
    /// `RECOGNITION` `journal_entry` ids (bounded to the recognition domain — one
    /// per release/reversal, the same cardinality as the DONE segments), then (2)
    /// their `REVENUE` `journal_line`s via `entry_id IN (…)`. The grouping key
    /// `(period_id, revenue_stream)` is a `BTreeMap` key, so the result is ordered
    /// by `(period_id, revenue_stream)`. Releases of a since-terminal schedule
    /// (`COMPLETED`/`REPLACED`/`CANCELLED`) still contribute — their journal entries
    /// are immutable historical fact, independent of the schedule's later lifecycle.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn list_revenue_disaggregation(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: Option<&str>,
    ) -> Result<Vec<RecognizedStreamEntry>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;

        // (1) The tenant's RECOGNITION entry ids, optionally narrowed to the period
        // (an entry's period == its lines' period, so this also prunes the line
        // scan). Scoped (SQL-level BOLA); bounded to the recognition domain.
        let mut entry_filter = Condition::all()
            .add(journal_entry::Column::TenantId.eq(tenant))
            .add(journal_entry::Column::SourceDocType.eq(SourceDocType::Recognition.as_str()));
        if let Some(period_id) = period_id {
            entry_filter = entry_filter.add(journal_entry::Column::PeriodId.eq(period_id));
        }
        let entry_ids: Vec<Uuid> = journal_entry::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(entry_filter)
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list recognition journal_entry: {e}")))?
            .into_iter()
            .map(|e| e.entry_id)
            .collect();
        if entry_ids.is_empty() {
            return Ok(Vec::new());
        }

        // (2) Their REVENUE lines. `journal_line.period_id` is the entry's period
        // (the actual open period on an E-2 missed-close); `revenue_stream` is the
        // per-stream tag both legs carry. Scoped.
        let lines = journal_line::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(journal_line::Column::TenantId.eq(tenant))
                    .add(journal_line::Column::AccountClass.eq(AccountClass::Revenue.as_str()))
                    .add(journal_line::Column::EntryId.is_in(entry_ids)),
            )
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("list recognition REVENUE journal_line: {e}")))?;

        // Net by (period_id, revenue_stream) → (Σ signed amount, currency). A
        // BTreeMap key orders the result by (period_id, revenue_stream).
        let mut grouped: std::collections::BTreeMap<(String, String), (i64, String)> =
            std::collections::BTreeMap::new();
        for line in lines {
            // Recognition REVENUE legs always tag a stream; skip an untagged line
            // defensively (no per-stream grain to attribute it to).
            let Some(stream) = line.revenue_stream.clone() else {
                continue;
            };
            // Signed: a CR REVENUE release recognizes (+), a DR REVENUE reversal
            // claws back (−).
            let signed = if line.side == Side::Credit.as_str() {
                line.amount_minor
            } else {
                -line.amount_minor
            };
            let entry = grouped
                .entry((line.period_id.clone(), stream))
                .or_insert_with(|| (0, line.currency.clone()));
            // i64 sum (the per-schedule cap CHECK bounds each schedule's release to
            // its deferred total; the per-account no-negative CHECK bounds the
            // aggregate — a single tenant/period/stream stays within i64).
            entry.0 = entry.0.saturating_add(signed);
        }
        Ok(grouped
            .into_iter()
            .map(
                |((period_id, revenue_stream), (recognized_minor, currency))| {
                    RecognizedStreamEntry {
                        period_id,
                        revenue_stream,
                        recognized_minor,
                        currency,
                    }
                },
            )
            .collect())
    }

    /// Enumerate the distinct `(tenant_id, period_id)` pairs that have at least
    /// one due releasable (`PENDING` or `QUEUED`) recognition segment — the
    /// **cross-tenant work feed** the Group F `RecognitionRunJob` ticker triggers a
    /// run for (one run per pair). An UNSCOPED, system-context read under the
    /// sanctioned all-tenants [`AccessScope::allow_all`] (the AM reaper / tie-out /
    /// queue-applier pattern), capped at `limit` rows scanned so a pathological
    /// backlog can't load an unbounded set into memory; the distinct pairs are
    /// folded in memory (the gear has no `DISTINCT`/`GROUP BY` access). A `QUEUED`
    /// segment's own period IS enumerated so the next tick re-runs it and drains
    /// the segment once its lower-period predecessor commits `DONE` (the runner's
    /// predecessor gate re-evaluates it); only `DONE` segments contribute no work.
    /// `effective_at` ordering is irrelevant — the per-pair run is idempotent and
    /// re-evaluates due work itself.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn list_due_tenant_periods(
        &self,
        limit: u64,
    ) -> Result<Vec<(Uuid, String)>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let segments = recognition_segment::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(
                Condition::all().add(
                    recognition_segment::Column::Status
                        .is_in([SEGMENT_STATUS_PENDING, SEGMENT_STATUS_QUEUED]),
                ),
            )
            .order_by(recognition_segment::Column::TenantId, Order::Asc)
            .order_by(recognition_segment::Column::PeriodId, Order::Asc)
            .limit(limit)
            .all(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("enumerate due tenant/periods: {e}")))?;
        // Distinct (tenant, period) pairs, folded in memory (no DB-side DISTINCT).
        let mut seen: std::collections::BTreeSet<(Uuid, String)> =
            std::collections::BTreeSet::new();
        for seg in segments {
            seen.insert((seg.tenant_id, seg.period_id));
        }
        Ok(seen.into_iter().collect())
    }

    // --- Ordering guard (Group E1, design §4.6) ---

    /// Count this schedule's segments with a lower `period_id` (an earlier
    /// period) that are NOT yet `DONE` — the **predecessor-not-done** check the
    /// runner consults before releasing a segment (design §4.6 ordering). A
    /// non-zero count means an earlier-period segment of the SAME schedule is
    /// still `PENDING`/`QUEUED`, so this segment must be parked `QUEUED` rather
    /// than released early. `period_id` is the `YYYYMM` lexical-sortable string,
    /// so a `<` string compare is the period-order compare (1:1 with
    /// `segment_no` within a schedule). Scoped (SQL-level BOLA); runs in the
    /// runner's read connection (not the post txn).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn count_predecessors_not_done(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
        period_id: &str,
    ) -> Result<u64, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let count = recognition_segment::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(recognition_segment::Column::TenantId.eq(tenant))
                    .add(recognition_segment::Column::ScheduleId.eq(schedule_id))
                    .add(recognition_segment::Column::PeriodId.lt(period_id))
                    .add(recognition_segment::Column::Status.ne(SEGMENT_STATUS_DONE)),
            )
            .count(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("count predecessors not done: {e}")))?;
        Ok(count)
    }

    /// Mark a `PENDING` segment `QUEUED` — the out-of-order park (design §4.6):
    /// a due segment whose lower-period predecessor is not yet `DONE` is not
    /// released, it is moved `PENDING → QUEUED` so a later run drains it once the
    /// predecessor commits. The filter requires the current status to be
    /// `PENDING` (a `QUEUED`/`DONE` segment is left untouched — `rows_affected ==
    /// 0` is then a benign no-op, not an error: a concurrent run may have already
    /// queued/released it). A scoped UPDATE outside any post txn (queuing posts
    /// no journal entry). `recognized_at`/`run_id` stay NULL — nothing was
    /// recognized.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn mark_segment_queued(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        schedule_id: &str,
        segment_no: i32,
    ) -> Result<(), RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        recognition_segment::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                recognition_segment::Column::Status,
                Expr::value(SEGMENT_STATUS_QUEUED.to_owned()),
            )
            .filter(
                Condition::all()
                    .add(recognition_segment::Column::TenantId.eq(tenant))
                    .add(recognition_segment::Column::ScheduleId.eq(schedule_id))
                    .add(recognition_segment::Column::SegmentNo.eq(segment_no))
                    .add(recognition_segment::Column::Status.eq(SEGMENT_STATUS_PENDING)),
            )
            .exec(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("mark recognition_segment QUEUED: {e}")))?;
        Ok(())
    }

    // --- Run-row orchestration (Group E2, design §4.3 / §7) ---

    /// Read the `recognition_run` row for `(tenant, period_id, run_id)`, or `None`
    /// when no such run exists — the run-trigger **dedup** read (design §4.3): a
    /// trigger
    /// whose `(tenant, period_id, run_id)` already has a row replays that run
    /// reference instead of starting a second run. SQL-level BOLA: a foreign
    /// tenant yields no row.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_run(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
        run_id: Uuid,
    ) -> Result<Option<recognition_run::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let row = recognition_run::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(recognition_run::Column::TenantId.eq(tenant))
                    .add(recognition_run::Column::PeriodId.eq(period_id))
                    .add(recognition_run::Column::RunId.eq(run_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read recognition_run: {e}")))?;
        Ok(row)
    }

    /// Read the `recognition_run` row for `(tenant, run_id)` — the
    /// `GET /recognition-runs/{run_id}` by-id source (read surface R4). The entity
    /// PK is the 3-column `(tenant_id, period_id, run_id)`, but the surrogate
    /// `run_id` is itself unique within a tenant in practice (the run-service mints
    /// a fresh `run_id` per run; the period is folded into the PK only to defend a
    /// client that REUSES one `run_id` across two periods), so
    /// a by-`run_id` read yields at most one row in the common case — and when a
    /// `run_id` WAS reused across periods this returns the first match (the by-id
    /// read is a single-run lookup, not the period-qualified dedup [`Self::read_run`]
    /// the run-service uses). Returns `None` when no run with that id exists for the
    /// tenant. Scoped (SQL-level BOLA — a foreign tenant yields `None`, the same 404
    /// as absent, no existence leak). Out-of-txn on a fresh scoped connection (a
    /// pure read). Mirrors `AdjustmentRepo::read_refund_out_of_txn` (filtered on the
    /// `Uuid` `run_id` rather than a `varchar` id).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn read_run_out_of_txn(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        run_id: Uuid,
    ) -> Result<Option<recognition_run::Model>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        recognition_run::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(recognition_run::Column::TenantId.eq(tenant))
                    .add(recognition_run::Column::RunId.eq(run_id)),
            )
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read recognition_run by run_id: {e}")))
    }

    /// List the `recognition_run` record rows for `tenant` under `scope`,
    /// cursor-paginated via the canonical `query` (`$filter` over `run_id` /
    /// `period_id` / `status`, `$orderby` / `limit` / `cursor`). The tenant
    /// predicate is pre-applied to the secured select; the user `$filter` is
    /// additive over it (SQL-level BOLA — a foreign value still ANDs the scope, so a
    /// cross-tenant run never leaks). A bare list defaults to `run_id ASC`. The
    /// `GET /recognition-runs` read-surface source; out-of-txn on a fresh scoped
    /// connection. Mirrors `AdjustmentRepo::list_refunds`.
    ///
    /// # Errors
    /// [`OdataPageError::Db`] on a storage / connection failure;
    /// [`OdataPageError::Odata`] on a malformed `$filter` / `$orderby` / cursor
    /// (the caller projects it to a canonical 400).
    pub async fn list_runs(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        query: &ODataQuery,
    ) -> Result<Page<recognition_run::Model>, OdataPageError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| OdataPageError::Db(format!("conn: {e}")))?;
        // Pre-apply the tenant predicate to the secured select; the user `$filter`
        // is applied additively by `paginate_odata` (it never replaces this scope —
        // BOLA preserved).
        let base_select = recognition_run::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(recognition_run::Column::TenantId.eq(tenant)));
        let query = query_with_default_order(query, "run_id");
        paginate_odata::<
            RecognitionRunFilterField,
            RecognitionRunODataMapper,
            recognition_run::Entity,
            recognition_run::Model,
            _,
            _,
        >(
            base_select,
            &conn,
            &query,
            ("run_id", SortDir::Asc),
            LimitCfg {
                default: 25,
                max: 200,
            },
            |m| m,
        )
        .await
        .map_err(map_odata_err)
    }

    /// Insert a fresh `recognition_run` row in state `RUNNING` (design §4.3):
    /// the orchestration wrapper around a [`RecognitionRunner`] pass. The run is
    /// **not** itself the at-most-once dedup key (that is the per-segment
    /// `RECOGNITION` gate); this row records the run for dedup + the
    /// single-active-run guard + audit. The PK `(tenant, period_id, run_id)` makes
    /// a duplicate run within one period collide — but the run-service dedups on a
    /// prior [`Self::read_run`] before inserting, so a collision here surfaces as
    /// [`RepoError::Db`]. A scoped insert (not in a post txn — the run row
    /// brackets the runner, it is not part of any single segment's post).
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn insert_run(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        run_id: Uuid,
        period_id: &str,
        started_at_utc: OffsetDateTime,
    ) -> Result<(), RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let am = recognition_run::ActiveModel {
            tenant_id: Set(tenant),
            period_id: Set(period_id.to_owned()),
            run_id: Set(run_id),
            started_at_utc: Set(started_at_utc),
            status: Set(RUN_STATUS_RUNNING.to_owned()),
        };
        recognition_run::Entity::insert(am.clone())
            .secure()
            .scope_with_model(scope, &am)
            .map_err(|e| RepoError::Db(format!("recognition_run scope: {e}")))?
            .exec(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("insert recognition_run: {e}")))?;
        Ok(())
    }

    /// Finish a `recognition_run`: flip `RUNNING → DONE` (`done = true`) or
    /// `RUNNING → FAILED` (`done = false`) for `(tenant, period_id, run_id)`
    /// (design §4.3). The filter carries the FULL 3-column key:
    /// `run_id` alone is not unique — a client may reuse one `run_id` across two
    /// periods (then BOTH run rows exist), so a `(tenant, run_id)` filter would flip
    /// the sibling period's run row too. The filter also requires the current status
    /// to be `RUNNING`, so a double-finish matches no row (`rows_affected == 0`) and
    /// is a benign no-op (the run already reached a terminal state). A scoped UPDATE.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn finish_run(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
        run_id: Uuid,
        done: bool,
    ) -> Result<(), RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let next = if done {
            RUN_STATUS_DONE
        } else {
            RUN_STATUS_FAILED
        };
        recognition_run::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                recognition_run::Column::Status,
                Expr::value(next.to_owned()),
            )
            .filter(
                Condition::all()
                    .add(recognition_run::Column::TenantId.eq(tenant))
                    .add(recognition_run::Column::PeriodId.eq(period_id))
                    .add(recognition_run::Column::RunId.eq(run_id))
                    .add(recognition_run::Column::Status.eq(RUN_STATUS_RUNNING)),
            )
            .exec(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("finish recognition_run: {e}")))?;
        Ok(())
    }

    // --- Current open period lookup (Group E3 missed-close, design §4.3 E-2) ---

    /// The tenant's current OPEN fiscal period id (`YYYYMM`), or `None` when the
    /// tenant has no open period — the **missed-close** target the runner falls
    /// back to (design §4.3 E-2): a segment whose own target period is CLOSED
    /// posts into the current open period instead (with its original target
    /// recorded as audit linkage), never into a closed period. v1 has one legal
    /// entity per tenant (the LE = the tenant), so the lookup keys on
    /// `(tenant, legal_entity = tenant)`; the **lowest** `period_id` among the
    /// tenant's OPEN periods is the current open period (periods open
    /// chronologically and close oldest-first, so the earliest still-OPEN one is
    /// "current"). Scoped (SQL-level BOLA).
    ///
    /// This is a minimal lookup the gear lacks otherwise (the foundation's
    /// [`FiscalPeriodGuard`](crate::infra::posting::period::FiscalPeriodGuard)
    /// only *asserts* a given period is OPEN; it does not *select* the current
    /// open one) — flagged for the controller: confirm the "lowest OPEN
    /// `period_id` = current" convention against the foundation's period-open job
    /// semantics.
    ///
    /// # Errors
    /// [`RepoError::Db`] on a scope or storage failure.
    pub async fn current_open_period(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
    ) -> Result<Option<String>, RepoError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| RepoError::Db(format!("conn: {e}")))?;
        let row = fiscal_period::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(
                Condition::all()
                    .add(fiscal_period::Column::TenantId.eq(tenant))
                    // v1: one legal entity per tenant (LE = tenant).
                    .add(fiscal_period::Column::LegalEntityId.eq(tenant))
                    .add(fiscal_period::Column::Status.eq(PERIOD_STATUS_OPEN)),
            )
            .order_by(fiscal_period::Column::PeriodId, Order::Asc)
            .one(&conn)
            .await
            .map_err(|e| RepoError::Db(format!("read current open period: {e}")))?;
        Ok(row.map(|p| p.period_id))
    }
}

/// Map a `SecureORM` [`ScopeError`] into a [`DbError`], PRESERVING the inner
/// `sea_orm::DbErr` so a serialization failure (`40001`) raised at a scoped read
/// / write inside the Group H change txn stays retryable via the retry helper's
/// `as_db_err`. Mirrors [`crate::infra::period_close`]'s `scope_to_db`. The
/// scope-validation variants (which never carry a driver error) become a
/// non-retryable `DbError::Other`. Used by the in-txn change helpers
/// (`read_schedule_in_txn`, `mark_schedule_status`, `insert_segments`, …); the
/// `add_recognized` cap path keeps its own [`map_cap_violation`] (it must classify
/// the over-recognition CHECK, not retryability).
fn scope_to_db(e: ScopeError) -> DbError {
    match e {
        ScopeError::Db(db_err) => DbError::Sea(db_err),
        other => DbError::Other(anyhow::anyhow!("scope: {other}")),
    }
}

/// Map a counter-write [`ScopeError`] to [`RepoError`]: a CHECK-constraint
/// violation (the per-obligation over-recognition cap) becomes
/// [`RepoError::MoneyOutCapExceeded`]; anything else stays a plain
/// [`RepoError::Db`]. Mirrors `PaymentRepo::map_cap_violation`. Only
/// `ScopeError::Db` can carry a driver CHECK error; the scope-validation
/// variants never do.
fn map_cap_violation(context: &str, err: &ScopeError) -> RepoError {
    if let ScopeError::Db(db_err) = err
        && is_check_violation(db_err)
    {
        return RepoError::MoneyOutCapExceeded(format!("{context}: {err}"));
    }
    RepoError::Db(format!("{context}: {err}"))
}

/// Returns `true` iff `err` is a `CHECK`-constraint violation on either
/// supported backend. Replicates `PaymentRepo::is_check_violation` (a private
/// fn in that module) with the recognition constraint-name prefix added:
/// `sea_orm::SqlErr` has no `Check` discriminant, so a real CHECK violation
/// always surfaces unstructured (`sql_err() == None`); a structured error
/// (unique / FK) is therefore never a CHECK and is refused here. The constraint
/// NAME is the most stable signal — the over-recognition cap
/// (`chk_ledger_recognition_schedule_recognized_le_deferred`, prefix
/// `chk_ledger_recognition_schedule_`) is the only recognition constraint whose
/// violation must map to `MoneyOutCapExceeded`, so match it by name first and
/// keep the SQLSTATE-anchored fallbacks (Postgres `23514`, `SQLite` extended
/// code `275`) as a backstop.
fn is_check_violation(err: &DbErr) -> bool {
    if err.sql_err().is_some() {
        return false;
    }
    let msg = err.to_string().to_lowercase();
    if msg.contains("chk_ledger_recognition_schedule_") {
        return true;
    }
    msg.contains("check constraint")
        || msg.contains("check_violation")
        || msg.contains("sqlite_constraint_check")
        || msg.contains("sqlstate 23514")
        || msg.contains("sqlstate: 23514")
        || msg.contains("sqlstate=23514")
        || msg.contains("code 23514")
        || msg.contains("code: 23514")
        || msg.contains("(23514)")
        || msg.contains("(23514:")
        || msg.starts_with("23514:")
        || msg.contains(" 23514:")
        || (msg.contains("sqlite")
            && (msg.contains("code 275")
                || msg.contains("code: 275")
                || msg.contains("(275)")
                || msg.contains("(275:")))
}
