//! `PeriodCloseService` — the gated `OPEN→CLOSED` fiscal-period transition
//! (Slice 7 Group B). The close is **single-active** via a `coord` lease
//! (keyed `period-close:{tenant}:{legal_entity}:{period}`; a concurrent close
//! sees [`CoordError::LeaseHeld`] → [`DomainError::PeriodCloseInProgress`]), and
//! under the lease the read, the gate, and the flip run in ONE `SERIALIZABLE`
//! transaction with retry — so a post that lands an entry into the period
//! concurrently conflicts (Postgres SSI) and the close retries against the new
//! entry instead of certifying a period the entry slipped into.
//!
//! Close is **single-tenant**: everything runs under
//! `AccessScope::for_tenant(tenant_id)` plus a `(legal_entity, period)` filter
//! (NOT the cross-tenant system scope the daily jobs use).
//!
//! The **gate** blocks close while any of: the pre-close tie-out is not clean
//! (variance / imbalance / negative / PENDING mapping line); an OPEN
//! close-blocking `exception_queue` row exists for the period; a recognition
//! segment due `<=` the period is not `DONE`; or — Slice 7 Phase 3, behind their
//! enforcement flags — the period's **bill run is not asserted finished** or the
//! independent **issued-invoice manifest** shows issued invoices with no committed
//! posting (a configured-but-absent/failing feed fails loud) (design §4.5). A blocked close
//! records `period_close = CLOSING` + `blocked_reasons` (observability) and
//! returns [`DomainError::PeriodCloseBlocked`]; a clean close flips
//! `fiscal_period OPEN→CLOSED` and `period_close → CLOSED` in the same commit and
//! emits `billing.ledger.period.closed` in-txn (transactional outbox).
//!
//! **Realized as `coord` lease + SERIALIZABLE/SSI** rather than the design's
//! literal `fiscal_period FOR UPDATE` two-phase (recompute-outside-lock +
//! watermark): the toolkit's `SecureORM` does not expose row locks, and SSI is
//! correctness-equivalent for the close-vs-post race (the lease adds
//! single-active mutual exclusion). The recompute-outside-lock optimization is a
//! deferred follow-up (spec §3.1/§8). REOPEN + dual-control land in Group B-reopen.

use std::sync::Arc;
use std::time::Duration;

use coord::{CoordError, LeaseGuard, LeaseManager};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{
    AccessScope, DbTx, ScopeError, SecureEntityExt, SecureUpdateExt, TxConfig,
};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use bss_ledger_sdk::{
    BillRunFinishedV1, CloseOutcome, IssuedInvoiceManifestV1, UnconfiguredBillRunFinishedV1,
    UnconfiguredIssuedInvoiceManifestV1,
};

use crate::domain::error::DomainError;
use crate::domain::fx::revaluation_mode::RevaluationMode;
use crate::domain::model::RepoError;
use crate::domain::status::{PERIOD_STATUS_CLOSED, PERIOD_STATUS_OPEN};
use crate::infra::audit::secured_audit_sink::{AuditEventType, SecuredAuditSink};
use crate::infra::events::payloads::{
    AlarmCategory, AlarmSeverity, LedgerInvariantAlarm, LedgerPeriodClosed,
};
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::jobs::tieout::TieOutJob;
use crate::infra::storage::entity::fiscal_period;
use crate::infra::storage::repo::{
    ExceptionQueueRepo, FxRevaluationModeRepo, FxRevaluationRunRepo, PeriodCloseRepo,
    RecognitionRepo,
};
use time::OffsetDateTime;

/// `period_close.status` literal for an attempted-but-blocked / in-flight close.
const PERIOD_CLOSE_STATUS_CLOSING: &str = "CLOSING";

/// The Slice 7 Phase 3 pre-close control-feed gate inputs (design §4.5 / decision 3):
/// the two launch-blocking control feeds (issued-invoice manifest, bill-run-finished)
/// plus their per-deployment enforcement flags. Built in `init()` from `ClientHub`
/// (the fail-safe `Unconfigured…` defaults) and `ReconConfig`. With a flag OFF the
/// corresponding gate is inert; with it ON, an absent / failing feed **fails loud**
/// (blocks close), never silently passes (decision 3). [`Self::inert`] disables both
/// gates (the reopen-only executor instance + tests that do not exercise completeness).
#[derive(Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "three independent per-deployment close-enforcement flags (manifest, bill-run, fx-revaluation)"
)]
pub struct CloseControlFeeds {
    /// The issued-invoice manifest read port (invoice-completeness pre-close gate).
    pub manifest_feed: Arc<dyn IssuedInvoiceManifestV1>,
    /// The bill-run-finished read port (close-after-bill-run gate, N-core-8 / S7-F1).
    pub bill_run_feed: Arc<dyn BillRunFinishedV1>,
    /// Block close on an unresolved invoice-completeness gap (`manifest_enforcement`).
    pub manifest_enforcement: bool,
    /// Block close until the bill-run-finished signal is asserted (`bill_run_enforcement`).
    pub bill_run_enforcement: bool,
    /// Block close until the period-end Mode-B FX revaluation recorded a COMPLETE
    /// marker (`fx.revaluation_enabled`, VHP-1859 review C3). Inert while Mode-B is
    /// off (the v1 default).
    pub fx_revaluation_enforcement: bool,
}

impl CloseControlFeeds {
    /// Both gates disabled with fail-safe `Unconfigured…` feeds — the close gate's
    /// completeness/bill-run checks are inert (the default before any feed is wired).
    #[must_use]
    pub fn inert() -> Self {
        Self {
            manifest_feed: Arc::new(UnconfiguredIssuedInvoiceManifestV1),
            bill_run_feed: Arc::new(UnconfiguredBillRunFinishedV1),
            manifest_enforcement: false,
            bill_run_enforcement: false,
            fx_revaluation_enforcement: false,
        }
    }
}

/// Lease TTL for a close — comfortably longer than the renewal so one missed
/// heartbeat does not drop it, yet short enough that a crashed close's lease
/// lapses (and the period is re-closable) within a couple of minutes.
const CLOSE_LEASE_TTL: Duration = Duration::from_mins(2);
/// Renewal heartbeat period (~`TTL`/3): a live close renews well before expiry.
const CLOSE_LEASE_RENEW: Duration = Duration::from_secs(40);

/// Carries the in-transaction close decision out of the `SERIALIZABLE` body.
/// Business outcomes are `Ok(_)` (the txn commits — only the guarded flip and the
/// `period_close` row write); infrastructure / serialization faults are `Err`.
enum CloseTxnResult {
    NotFound,
    AlreadyClosed,
    NotOpen,
    /// Gate blocked the close; the `period_close` row was written `CLOSING` with
    /// these reasons (observability). Mapped to [`DomainError::PeriodCloseBlocked`].
    Blocked(Vec<String>),
    Flipped {
        already_closed: bool,
    },
}

/// Carries the in-transaction reopen decision out of the `SERIALIZABLE` body.
enum ReopenTxnResult {
    NotFound,
    /// Already OPEN (never closed, or a prior reopen) — idempotent no-op.
    NoOp,
    Reopened,
}

/// Extractor for the retry helper: a wrapped `DbErr` (so a serialization failure
/// surfaced at a statement or COMMIT is recognised as retryable).
fn as_db_err(e: &DbError) -> Option<&sea_orm::DbErr> {
    match e {
        DbError::Sea(db_err) => Some(db_err),
        _ => None,
    }
}

/// Map a `SecureORM` error into `DbError`, preserving the inner `DbErr` so a
/// serialization failure at a scoped read / flip stays retryable via
/// [`as_db_err`].
fn scope_to_db(e: ScopeError) -> DbError {
    match e {
        ScopeError::Db(db_err) => DbError::Sea(db_err),
        other => DbError::Other(anyhow::anyhow!("scope: {other}")),
    }
}

/// Map a repo error into `DbError`. The gate's `exception_queue` / `period_close`
/// statements are serialised against peer closes by the `coord` lease, so the
/// lost-retryability of a stringified repo error here is benign (a rare conflict
/// fails the close; the caller retries). The post-vs-close race is caught by the
/// tie-out reads + the flip, which preserve the retryable `DbErr` via
/// [`scope_to_db`].
#[allow(
    clippy::needless_pass_by_value,
    reason = "error adapter used as a map_err fn-pointer; takes the error by value to match the closure signature"
)]
fn repo_to_db(e: RepoError) -> DbError {
    DbError::Other(anyhow::anyhow!("period-close repo: {e}"))
}

/// Closes a clean fiscal period after a single-active, gated pre-close check;
/// also owns the dual-control `reopen` (the CLOSED→REOPENED seam).
pub struct PeriodCloseService {
    db: DBProvider<DbError>,
    publisher: Arc<LedgerEventPublisher>,
    /// Single-active-close lease (design §4.5). Keyed
    /// `period-close:{tenant}:{legal_entity}:{period}`, built over the same `Db`
    /// as the repos (mirrors `RecognitionRunService`).
    lease: LeaseManager,
    /// Secured-audit sink for the `period-reopen` record (NO-OP until Slice 6;
    /// the close path itself writes no audit record).
    audit: Arc<dyn SecuredAuditSink>,
    /// Slice 7 Phase 3 pre-close control-feed gate (manifest completeness + bill-run
    /// finished, flag-gated). `None` ⇒ inert (the reopen-only executor instance); the
    /// close path attaches the real feeds via [`Self::with_control_feeds`].
    control: Option<CloseControlFeeds>,
}

impl PeriodCloseService {
    /// Build the service over one database provider and the event publisher
    /// (threaded into the pre-close [`TieOutJob`]); the lease manager is built
    /// from the provider's `Db` (`db.db()`), mirroring `RecognitionRunService`.
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        audit: Arc<dyn SecuredAuditSink>,
    ) -> Self {
        let lease = LeaseManager::new(db.db());
        Self {
            db,
            publisher,
            lease,
            audit,
            control: None,
        }
    }

    /// Attach the Slice 7 Phase 3 pre-close control-feed gate (manifest completeness +
    /// bill-run-finished, flag-gated — design §4.5). The close path (the in-process
    /// client's instance) sets this; the reopen-only executor instance leaves it inert.
    #[must_use]
    pub fn with_control_feeds(mut self, control: CloseControlFeeds) -> Self {
        self.control = Some(control);
        self
    }

    /// Close the `(tenant, legal_entity, period)` fiscal period: take the
    /// single-active lease, then under it assert `OPEN`, run the gate, and flip
    /// to `CLOSED`. The caller's `ctx` threads the initiating Finance actor onto
    /// the `period_close` row (`initiated_by`) and the `period.closed` event.
    ///
    /// Idempotent — a period that is already `CLOSED` returns
    /// `Ok(already_closed = true)` without re-running the gate or re-emitting the
    /// `period.closed` event.
    ///
    /// # Errors
    /// [`DomainError::PeriodCloseInProgress`] when a peer close holds the lease;
    /// [`DomainError::PeriodCloseBlocked`] when the gate blocks (tie-out variance
    /// / open exception / due recognition segment); [`DomainError::PeriodNotFound`]
    /// when the row is absent; [`DomainError::PeriodNotOpen`] when the period is
    /// neither `OPEN` nor `CLOSED`; [`DomainError::Internal`] on a storage /
    /// lease failure.
    pub async fn close(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        legal_entity_id: Uuid,
        period_id: String,
    ) -> Result<CloseOutcome, DomainError> {
        let lease_key = format!("period-close:{tenant_id}:{legal_entity_id}:{period_id}");
        let guard = match self.lease.acquire(&lease_key, CLOSE_LEASE_TTL).await {
            Ok(guard) => guard,
            Err(CoordError::LeaseHeld) => {
                return Err(DomainError::PeriodCloseInProgress(format!(
                    "{legal_entity_id}/{period_id}"
                )));
            }
            Err(e) => {
                return Err(DomainError::Internal(format!(
                    "period-close lease acquire ({lease_key}): {e}"
                )));
            }
        };

        // Keep the lease live across the gate (the tie-out can be heavy); stop the
        // heartbeat before release.
        let renewal = guard.spawn_renewal(CLOSE_LEASE_RENEW);
        let result = self
            .close_locked(ctx, tenant_id, legal_entity_id, period_id)
            .await;
        renewal.shutdown().await;
        Self::release_lease(guard, result.is_ok()).await;
        result
    }

    /// The gated close body under a held lease: one `SERIALIZABLE` transaction
    /// (with retry) carrying the read → gate → flip → `period.closed` emit, mapped
    /// onto the SDK outcome.
    async fn close_locked(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        legal_entity_id: Uuid,
        period_id: String,
    ) -> Result<CloseOutcome, DomainError> {
        let publisher = Arc::clone(&self.publisher);
        let db = self.db.clone();
        let pid = period_id.clone();
        // Owned for the retryable `move` closure (a borrow of `ctx` cannot enter it);
        // re-cloned per retry attempt below, mirroring `publisher` / `db` / `pid`.
        let ctx = ctx.clone();
        // Slice 7 Phase 3 pre-close control-feed gate inputs (manifest + bill-run,
        // flag-gated); `None` ⇒ the gate is inert. Cheap to clone (`Arc`s + bools).
        let control = self.control.clone();

        let result: Result<CloseTxnResult, DbError> = self
            .db
            .db()
            .transaction_with_retry(TxConfig::serializable(), as_db_err, move |txn| {
                let publisher = Arc::clone(&publisher);
                let db = db.clone();
                let pid = pid.clone();
                let ctx = ctx.clone();
                let control = control.clone();
                Box::pin(async move {
                    close_in_txn(
                        &ctx,
                        txn,
                        &db,
                        &publisher,
                        control.as_ref(),
                        tenant_id,
                        legal_entity_id,
                        &pid,
                    )
                    .await
                })
            })
            .await;

        match result {
            Ok(CloseTxnResult::NotFound) => Err(DomainError::PeriodNotFound(period_id)),
            Ok(CloseTxnResult::AlreadyClosed) => Ok(CloseOutcome {
                period_id,
                already_closed: true,
            }),
            Ok(CloseTxnResult::NotOpen) => Err(DomainError::PeriodNotOpen(
                "fiscal period is not OPEN".to_owned(),
            )),
            Ok(CloseTxnResult::Blocked(reasons)) => {
                Err(DomainError::PeriodCloseBlocked(reasons.join("; ")))
            }
            Ok(CloseTxnResult::Flipped { already_closed }) => Ok(CloseOutcome {
                period_id,
                already_closed,
            }),
            Err(e) => Err(DomainError::Internal(format!("close txn: {e}"))),
        }
    }

    /// Reopen a CLOSED fiscal period (design §7 / N-core-3) — the last
    /// dual-control seam (VHP-1852). Flips `fiscal_period CLOSED→OPEN` and
    /// `period_close → REOPENED`, and writes a Slice-6 `period-reopen`
    /// secured-audit record, all in one `SERIALIZABLE` txn under the single-active
    /// close lease. Called ONLY by the dual-control executor on approve (reopen is
    /// ALWAYS dual-control, policy `requires_dual_control`); never inline.
    /// Idempotent — an already-OPEN period is a no-op success.
    ///
    /// # Errors
    /// [`DomainError::PeriodCloseInProgress`] on a held lease;
    /// [`DomainError::PeriodNotFound`] when the row is absent;
    /// [`DomainError::Internal`] on a storage / lease failure.
    pub async fn reopen(
        &self,
        tenant_id: Uuid,
        legal_entity_id: Uuid,
        period_id: &str,
        actor: Uuid,
    ) -> Result<(), DomainError> {
        let lease_key = format!("period-close:{tenant_id}:{legal_entity_id}:{period_id}");
        let guard = match self.lease.acquire(&lease_key, CLOSE_LEASE_TTL).await {
            Ok(guard) => guard,
            Err(CoordError::LeaseHeld) => {
                return Err(DomainError::PeriodCloseInProgress(format!(
                    "{legal_entity_id}/{period_id}"
                )));
            }
            Err(e) => {
                return Err(DomainError::Internal(format!(
                    "period-reopen lease acquire ({lease_key}): {e}"
                )));
            }
        };
        let renewal = guard.spawn_renewal(CLOSE_LEASE_RENEW);
        let result = self
            .reopen_locked(tenant_id, legal_entity_id, period_id, actor)
            .await;
        renewal.shutdown().await;
        Self::release_lease(guard, result.is_ok()).await;
        result
    }

    /// The reopen body under a held lease: one `SERIALIZABLE` transaction (with
    /// retry) carrying the read → flip → secured-audit.
    async fn reopen_locked(
        &self,
        tenant_id: Uuid,
        legal_entity_id: Uuid,
        period_id: &str,
        actor: Uuid,
    ) -> Result<(), DomainError> {
        let audit = Arc::clone(&self.audit);
        let pid = period_id.to_owned();

        let result: Result<ReopenTxnResult, DbError> = self
            .db
            .db()
            .transaction_with_retry(TxConfig::serializable(), as_db_err, move |txn| {
                let audit = Arc::clone(&audit);
                let pid = pid.clone();
                Box::pin(async move {
                    reopen_in_txn(txn, &audit, tenant_id, legal_entity_id, &pid, actor).await
                })
            })
            .await;

        match result {
            Ok(ReopenTxnResult::NotFound) => Err(DomainError::PeriodNotFound(period_id.to_owned())),
            Ok(ReopenTxnResult::NoOp | ReopenTxnResult::Reopened) => Ok(()),
            Err(e) => Err(DomainError::Internal(format!("reopen txn: {e}"))),
        }
    }

    /// Release the close lease, logging (not failing) a release fault — the lease
    /// lapses at TTL regardless. A failed close uses `release_with_retry` to free
    /// the slot promptly so a retry is not blocked by a stale lease.
    async fn release_lease(guard: LeaseGuard, succeeded: bool) {
        let key = guard.key().to_owned();
        let released = if succeeded {
            guard.release().await
        } else {
            guard.release_with_retry().await
        };
        if let Err(e) = released {
            tracing::warn!(
                target: "bss-ledger",
                error = %e,
                lease_key = %key,
                "failed to release period-close lease (will lapse at TTL)"
            );
        }
    }
}

/// In-transaction close body: read (OPEN?) → gate (tie-out + open exceptions +
/// due recognition segments) → on block persist `period_close = CLOSING` +
/// reasons; else flip `fiscal_period OPEN→CLOSED` + `period_close → CLOSED` and
/// emit `period.closed` (all in-txn, transactional outbox). Runs under the
/// caller's `SERIALIZABLE` transaction so the gate reads, the flip, and the event
/// share one snapshot and conflict with a concurrent post (SSI). The initiating
/// Finance actor (`ctx.subject_id()`) is stamped on the `period_close` row and the
/// event.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the in-txn close body threads the gate inputs (db/publisher/control-feeds) + the (tenant, legal_entity, period) coordinate through one serializable transaction; the gated sequence (tie-out, exceptions, recognition, control feeds, fx-revaluation, flip, emit) reads top-to-bottom as one cohesive unit"
)]
async fn close_in_txn(
    ctx: &SecurityContext,
    txn: &DbTx<'_>,
    db: &DBProvider<DbError>,
    publisher: &Arc<LedgerEventPublisher>,
    control: Option<&CloseControlFeeds>,
    tenant_id: Uuid,
    legal_entity_id: Uuid,
    period_id: &str,
) -> Result<CloseTxnResult, DbError> {
    let scope = AccessScope::for_tenant(tenant_id);
    let actor = ctx.subject_id();
    let actor_s = actor.to_string();

    // 1. Read the period row (in-txn → joins the serializable snapshot).
    let row = fiscal_period::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(fiscal_period::Column::TenantId.eq(tenant_id))
                .add(fiscal_period::Column::LegalEntityId.eq(legal_entity_id))
                .add(fiscal_period::Column::PeriodId.eq(period_id.to_owned())),
        )
        .one(txn)
        .await
        .map_err(scope_to_db)?;

    match row {
        None => return Ok(CloseTxnResult::NotFound),
        Some(p) if p.status == PERIOD_STATUS_CLOSED => return Ok(CloseTxnResult::AlreadyClosed),
        Some(p) if p.status == PERIOD_STATUS_OPEN => {}
        Some(_) => return Ok(CloseTxnResult::NotOpen),
    }

    // 2. Gate — accumulate blocking reasons (all in this txn so a concurrent post
    // conflicts via SSI).
    let mut reasons: Vec<String> = Vec::new();

    // 2a. Pre-close tie-out (variance / imbalance / negative / PENDING mapping).
    let report = TieOutJob::new(db.clone(), Arc::clone(publisher))
        .tie_out_on(txn, tenant_id)
        .await
        .map_err(|e| DbError::Other(anyhow::anyhow!("pre-close tie-out: {e}")))?;
    if !report.is_clean() {
        reasons.push(format!("tie-out not clean: {}", report.summary()));
    }

    // 2b. OPEN close-blocking exceptions for the period (APPROVED_EXCEPTION rows
    // are not OPEN, so a Finance-acknowledged GL-writeoff variance is excluded).
    let open = ExceptionQueueRepo::list_open_in_txn(txn, &scope, tenant_id, period_id)
        .await
        .map_err(repo_to_db)?;
    if !open.is_empty() {
        let listed: Vec<String> = open
            .iter()
            .map(|e| format!("{}({})", e.exception_type, e.business_ref))
            .collect();
        reasons.push(format!(
            "{} open exception(s): {}",
            open.len(),
            listed.join(", ")
        ));
    }

    // 2c. Recognition segments due `<=` this period that have not released.
    let due_not_done =
        RecognitionRepo::count_due_not_done_in_txn(txn, &scope, tenant_id, period_id).await?;
    if due_not_done > 0 {
        reasons.push(format!(
            "{due_not_done} recognition segment(s) due <= {period_id} not DONE"
        ));
    }

    // 2d. (The Mode-B FX-revaluation-incomplete gate — design §4.5 / PRD F7 — is the
    // `control.fx_revaluation_enforcement` block below, resolved PER-TENANT (VHP-1986):
    // only an effective-Mode-B tenant must have a COMPLETE marker. Inert while
    // enforcement is off, the v1 default.)

    // 2e. (Slice 7 Phase 3) the mandatory pre-close control-feed gates (design §4.5 /
    // decision 3), each behind its enforcement flag. A configured-but-absent/failing
    // feed FAILS LOUD (blocks), never silently passes. Both are inert by default (the
    // feeds are launch-blocking cross-team; flags OFF until live), so completeness then
    // leans on runbook discipline (the stated residual MVP risk).
    if let Some(control) = control {
        // Close-after-bill-run (N-core-8 / S7-F1): block until the period's bill run is
        // asserted finished. `None` (not asserted) and `Some(false)` both block when ON.
        if control.bill_run_enforcement {
            match control
                .bill_run_feed
                .is_finished(tenant_id, period_id)
                .await
            {
                Ok(Some(true)) => {}
                Ok(Some(false)) => reasons.push("bill run not finished for the period".to_owned()),
                Ok(None) => {
                    reasons.push("bill-run-finished not asserted (enforcement on)".to_owned());
                }
                Err(e) => reasons.push(format!("bill-run feed error (fail-loud): {e}")),
            }
        }
        // Invoice-completeness (N-recon-1): the independent issued-invoice manifest vs
        // the posted `INVOICE_POST` set. Any issued invoice with no committed entry blocks.
        if control.manifest_enforcement {
            match control
                .manifest_feed
                .latest_manifest(tenant_id, period_id)
                .await
            {
                Ok(Some(manifest)) => {
                    let posted = crate::infra::reconciliation::posted_invoice_ids(
                        txn, &scope, tenant_id, period_id,
                    )
                    .await?;
                    let missing = manifest
                        .invoice_ids
                        .iter()
                        .filter(|id| !posted.contains(*id))
                        .count();
                    if missing > 0 {
                        reasons.push(format!(
                            "{missing} issued invoice(s) with no committed posting (missed-posting)"
                        ));
                    }
                    // Reconcile the manifest's count control-total (design §3.3): an
                    // INVOICE_POST not on the manifest (an extra / duplicate posting) or
                    // a count-inconsistent feed leaves `missing == 0` yet the posted set
                    // differs in size — block rather than silently certify a period whose
                    // posted invoices do not match what was billed. (Gross-total
                    // reconciliation needs a defined posted-gross-per-invoice semantic
                    // aligned with the issuer's `gross_total_minor`; tracked follow-up.)
                    let posted_count = u64::try_from(posted.len()).unwrap_or(u64::MAX);
                    if manifest.count != posted_count {
                        reasons.push(format!(
                            "issued-invoice count mismatch (manifest {}, posted {posted_count})",
                            manifest.count
                        ));
                    }
                }
                // Configured-but-missing fails loud (decision 3): a manifest must be
                // present once enforcement is on, or close cannot prove completeness.
                Ok(None) => {
                    reasons.push("issued-invoice manifest unavailable (enforcement on)".to_owned());
                }
                Err(e) => reasons.push(format!("manifest feed error (fail-loud): {e}")),
            }
        }
        // Mode-B FX-revaluation completeness (VHP-1859 review C3): when Mode-B is
        // enabled the period-end revaluation MUST have recorded a COMPLETE marker
        // for this period. Without it (a failed/lagged run) close BLOCKS and emits
        // FxRevaluationIncomplete — never certifying a period whose missing
        // FX_REVALUATION the closed-period guard would make unpostable forever.
        // Entry-existence cannot prove the run happened (a clean run legitimately
        // posts zero entries), so the marker is required.
        if control.fx_revaluation_enforcement {
            // VHP-1986: enforce the COMPLETE marker only for a tenant that is
            // effectively Mode B (BSS = ledger of record). An explicit Mode-A tenant
            // (its ERP revalues) is exempt; an unconfigured tenant follows the fleet
            // default (enforcement-on ⇒ Mode B).
            let mode = FxRevaluationModeRepo::read_effective_mode_in_txn(
                txn,
                &scope,
                tenant_id,
                OffsetDateTime::now_utc(),
            )
            .await
            .map_err(repo_to_db)?
            .unwrap_or(RevaluationMode::fleet_default(
                control.fx_revaluation_enforcement,
            ));
            let complete = if mode.revalues() {
                FxRevaluationRunRepo::is_period_complete(txn, &scope, tenant_id, period_id)
                    .await
                    .map_err(repo_to_db)?
            } else {
                // Mode A is exempt — the tenant's ERP revalues; nothing to require.
                true
            };
            if !complete {
                reasons.push(
                    "Mode-B FX revaluation not COMPLETE for the period (enforcement on)".to_owned(),
                );
                publisher
                    .emit_invariant_alarm(
                        ctx,
                        LedgerInvariantAlarm {
                            category: AlarmCategory::FxRevaluationIncomplete,
                            severity: AlarmSeverity::Critical,
                            tenant_id,
                            scope: format!("tenant:{tenant_id}"),
                            code: "FX_REVALUATION_INCOMPLETE".to_owned(),
                            detail: format!(
                                "period {period_id} closing without a COMPLETE Mode-B \
                                 revaluation marker"
                            ),
                            affected: Vec::new(),
                        },
                    )
                    .await;
            }
        }
    }

    // 3. Blocked → persist CLOSING + reasons for the dashboard, return Blocked.
    if !reasons.is_empty() {
        let blocked = serde_json::Value::Array(
            reasons
                .iter()
                .map(|r| serde_json::Value::String(r.clone()))
                .collect(),
        );
        PeriodCloseRepo::upsert_status(
            txn,
            &scope,
            tenant_id,
            legal_entity_id,
            period_id,
            PERIOD_CLOSE_STATUS_CLOSING,
            &actor_s,
            Some(blocked),
            None,
            None,
        )
        .await
        .map_err(repo_to_db)?;
        return Ok(CloseTxnResult::Blocked(reasons));
    }

    // 4. Clean → flip `fiscal_period OPEN→CLOSED` (guarded on status=OPEN) and
    // record `period_close = CLOSED` in the same commit.
    let res = fiscal_period::Entity::update_many()
        .secure()
        .scope_with(&scope)
        .col_expr(
            fiscal_period::Column::Status,
            Expr::value(PERIOD_STATUS_CLOSED),
        )
        .filter(
            Condition::all()
                .add(fiscal_period::Column::TenantId.eq(tenant_id))
                .add(fiscal_period::Column::LegalEntityId.eq(legal_entity_id))
                .add(fiscal_period::Column::PeriodId.eq(period_id.to_owned()))
                .add(fiscal_period::Column::Status.eq(PERIOD_STATUS_OPEN)),
        )
        .exec(txn)
        .await
        .map_err(scope_to_db)?;

    let closed_at = OffsetDateTime::now_utc();
    PeriodCloseRepo::upsert_status(
        txn,
        &scope,
        tenant_id,
        legal_entity_id,
        period_id,
        PERIOD_STATUS_CLOSED,
        &actor_s,
        None,
        None,
        Some(closed_at),
    )
    .await
    .map_err(repo_to_db)?;

    // Emit `period.closed` ONLY on a real `OPEN→CLOSED` flip (`rows_affected > 0`),
    // never on an idempotent no-op (a concurrent peer beat us to the flip under the
    // same lease — `rows_affected == 0`). In-txn (transactional outbox), so the
    // event commits atomically with the flip or rolls back with it.
    if res.rows_affected > 0 {
        // VHP-1843: snapshot the just-verified caches as the cumulative tie-out
        // baseline through this closing period — in the SAME txn, so it rolls back
        // with the close on abort. The pre-close tie-out (2a) proved the caches,
        // so they ARE the verified total through `period_id`; the daily / recon
        // incremental tie-out then verifies `baseline + fold(open) == cache`
        // instead of folding all-time. Only on a real OPEN→CLOSED flip (not an
        // idempotent re-entry — the prior close already wrote the baseline).
        TieOutJob::new(db.clone(), Arc::clone(publisher))
            .snapshot_baseline(txn, tenant_id, period_id)
            .await
            .map_err(|e| {
                DbError::Other(anyhow::anyhow!("close: snapshot tie-out baseline: {e}"))
            })?;
        publisher
            .publish_period_closed(
                ctx,
                txn,
                LedgerPeriodClosed {
                    tenant_id,
                    legal_entity_id,
                    period_id: period_id.to_owned(),
                    closed_by: actor,
                    closed_at_utc: closed_at,
                },
            )
            .await
            .map_err(|e| DbError::Other(anyhow::anyhow!("publish period.closed: {e}")))?;
    }

    Ok(CloseTxnResult::Flipped {
        already_closed: res.rows_affected == 0,
    })
}

/// In-transaction reopen body: read (CLOSED?) → flip `fiscal_period CLOSED→OPEN`
/// (guarded) + `period_close → REOPENED` → write the `period-reopen`
/// secured-audit record. Runs under the caller's `SERIALIZABLE` transaction.
async fn reopen_in_txn(
    txn: &DbTx<'_>,
    audit: &Arc<dyn SecuredAuditSink>,
    tenant_id: Uuid,
    legal_entity_id: Uuid,
    period_id: &str,
    actor: Uuid,
) -> Result<ReopenTxnResult, DbError> {
    let scope = AccessScope::for_tenant(tenant_id);

    let row = fiscal_period::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(fiscal_period::Column::TenantId.eq(tenant_id))
                .add(fiscal_period::Column::LegalEntityId.eq(legal_entity_id))
                .add(fiscal_period::Column::PeriodId.eq(period_id.to_owned())),
        )
        .one(txn)
        .await
        .map_err(scope_to_db)?;

    match row {
        None => return Ok(ReopenTxnResult::NotFound),
        // Already OPEN (never closed, or a prior reopen) — idempotent no-op.
        Some(p) if p.status == PERIOD_STATUS_OPEN => return Ok(ReopenTxnResult::NoOp),
        _ => {}
    }

    // Flip CLOSED→OPEN (guarded on status=CLOSED).
    fiscal_period::Entity::update_many()
        .secure()
        .scope_with(&scope)
        .col_expr(
            fiscal_period::Column::Status,
            Expr::value(PERIOD_STATUS_OPEN),
        )
        .filter(
            Condition::all()
                .add(fiscal_period::Column::TenantId.eq(tenant_id))
                .add(fiscal_period::Column::LegalEntityId.eq(legal_entity_id))
                .add(fiscal_period::Column::PeriodId.eq(period_id.to_owned()))
                .add(fiscal_period::Column::Status.eq(PERIOD_STATUS_CLOSED)),
        )
        .exec(txn)
        .await
        .map_err(scope_to_db)?;

    // Record the close-process row as REOPENED.
    let actor_s = actor.to_string();
    PeriodCloseRepo::upsert_status(
        txn,
        &scope,
        tenant_id,
        legal_entity_id,
        period_id,
        "REOPENED",
        &actor_s,
        None,
        None,
        None,
    )
    .await
    .map_err(repo_to_db)?;

    // Slice-6 `period-reopen` secured-audit record (NO-OP sink until Slice 6).
    let before_after = serde_json::json!({
        "period_id": period_id,
        "legal_entity_id": legal_entity_id.to_string(),
        "transition": "CLOSED->REOPENED",
    });
    audit
        .append(
            txn,
            &scope,
            tenant_id,
            AuditEventType::PeriodReopen,
            Some(actor_s.as_str()),
            Some("period-reopen"),
            &before_after,
            None,
            None,
        )
        .await?;

    Ok(ReopenTxnResult::Reopened)
}
