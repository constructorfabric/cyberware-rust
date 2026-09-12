//! `ReconciliationFramework` — the Slice 7 Phase 3 reconciliation engine (design §4.3).
//!
//! Runs the tenant-scoped reconciliation checks — **AR↔derived** (AC #7), **Payments↔PSP**,
//! and **invoice-completeness** (N-recon-1) — each producing a durable
//! `reconciliation_run` row with a variance result. An out-of-tolerance run opens a
//! close-blocking `exception_queue` row (via the [`ExceptionRouter`], additive +
//! fire-and-forget), raises the `ReconciliationVariance` / `MissedPosting` alarm, and
//! emits `billing.ledger.reconciliation.completed`. Slice 7 posts **no** financial
//! entries — it reads, reconciles, and gates close (design §0).
//!
//! Each check runs in ONE transaction: `start` the run (RUNNING) → read + compute the
//! variance → `finalize` (DONE) + emit the reconciliation-completed event,
//! all-or-nothing (default isolation; the SERIALIZABLE authority is the close gate, so a
//! recon-run is an audit record that self-heals via the tick). The close-blocking
//! exception + the
//! alarm are then raised out-of-band (their own transactions) — additive, never fails
//! the run.
//!
//! **Inert-until-the-feed-lands (decision 3).** The Payments↔PSP and invoice-completeness
//! checks read a control-feed port (`PspSettlementFeedV1` / `IssuedInvoiceManifestV1`):
//! an [`Unconfigured…`](bss_ledger_sdk::UnconfiguredIssuedInvoiceManifestV1) feed returns
//! `None` ⇒ the check is inert (no run, no block). A **configured** feed that errors
//! fails loud (the check returns `Err`), never silently passes. Whether a detected
//! invoice-completeness gap **blocks close** is gated by `manifest_enforcement` (default
//! OFF until the launch-blocking cross-team feed is live).

use std::collections::HashSet;
use std::sync::Arc;

use sea_orm::{ColumnTrait, Condition, EntityTrait, FromQueryResult, QuerySelect};
use serde_json::json;
use toolkit_db::secure::{AccessScope, DbTx, SecureEntityExt};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use bss_ledger_sdk::{IssuedInvoiceManifestV1, PspSettlementFeedV1, SourceDocType};

use crate::config::ReconConfig;
use crate::domain::error::DomainError;
use crate::domain::exception::ExceptionType;
use crate::domain::model::RepoError;
use crate::domain::ports::metrics::LedgerMetricsPort;
use crate::infra::events::alarm_catalog::severity;
use crate::infra::events::payloads::{
    AlarmCategory, LedgerInvariantAlarm, LedgerReconciliationCompleted,
};
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::exception::ExceptionRouter;
use crate::infra::jobs::tieout::{TieOutJob, TieOutReport};
use crate::infra::storage::entity::journal_entry;
use crate::infra::storage::repo::{
    ExceptionQueueRepo, JournalRepo, RecognitionRepo, ReconciliationRunRepo,
};
use time::OffsetDateTime;

/// `reconciliation_run.check_type` for the AR-ledger ↔ derived-projection tie-out (AC #7).
pub const CHECK_AR_DERIVED: &str = "AR_DERIVED";
/// `reconciliation_run.check_type` for the Payments ↔ PSP settlement tie.
pub const CHECK_PAYMENTS_PSP: &str = "PAYMENTS_PSP";
/// `reconciliation_run.check_type` for the upstream → ledger invoice-completeness check.
pub const CHECK_INVOICE_COMPLETENESS: &str = "INVOICE_COMPLETENESS";

/// `reconciliation_run.status` literal for a finalized (completed) run.
const RUN_STATUS_DONE: &str = "DONE";

/// Map a repo error into `DbError` for the in-txn run writes.
#[allow(
    clippy::needless_pass_by_value,
    reason = "error adapter used as a map_err fn-pointer; takes the error by value to match the closure signature"
)]
fn repo_to_db(e: RepoError) -> DbError {
    DbError::Other(anyhow::anyhow!("reconciliation repo: {e}"))
}

/// The decision carried out of a check's transaction to the out-of-band
/// metrics / exception-routing / alarm step.
struct ReconOutcome {
    variance_minor: i64,
    within_tolerance: bool,
}

/// Runs the Slice 7 reconciliation checks, writing `reconciliation_run` rows and
/// routing out-of-tolerance results to the close-blocking exception queue. Holds the
/// control-feed read ports (resolved from `ClientHub`, fail-safe `Unconfigured…`
/// defaults) and the exception router. Built once in `init()` over `db.clone()` (the
/// jobs pattern), shared by the `ReconciliationJob` ticker + the REST trigger.
pub struct ReconciliationFramework {
    db: DBProvider<DbError>,
    publisher: Arc<LedgerEventPublisher>,
    metrics: Arc<dyn LedgerMetricsPort>,
    exceptions: Arc<ExceptionRouter>,
    /// The exception-queue repo (list / resolve) — the invoice-completeness check
    /// auto-resolves a `MISSED_POSTING` once the missing invoice's idempotent re-post
    /// lands (design §4.6), alongside opening new ones via the router.
    exception_repo: ExceptionQueueRepo,
    manifest_feed: Arc<dyn IssuedInvoiceManifestV1>,
    psp_feed: Arc<dyn PspSettlementFeedV1>,
    /// `current_open_period` resolution for the ticker (reuses the recognition repo's
    /// fiscal-period read, like the `ExceptionRouter`).
    periods: RecognitionRepo,
    config: ReconConfig,
}

impl ReconciliationFramework {
    /// Build the framework over one database provider, the event publisher, the
    /// metrics sink, the exception router, the two control-feed read ports, and the
    /// recon config (tolerance + enforcement flags).
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
        exceptions: Arc<ExceptionRouter>,
        manifest_feed: Arc<dyn IssuedInvoiceManifestV1>,
        psp_feed: Arc<dyn PspSettlementFeedV1>,
        config: ReconConfig,
    ) -> Self {
        let periods = RecognitionRepo::new(db.clone());
        let exception_repo = ExceptionQueueRepo::new(db.clone());
        Self {
            db,
            publisher,
            metrics,
            exceptions,
            exception_repo,
            manifest_feed,
            psp_feed,
            periods,
            config,
        }
    }

    /// Run one named reconciliation check for `(tenant, period)` — the REST trigger
    /// entry (`POST /reconciliation-runs`) and the on-demand path. Returns the new
    /// `run_id`.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] for an unknown `check_type` or a check whose
    /// control feed is not configured (inert ⇒ nothing to reconcile); the underlying
    /// storage / feed error otherwise.
    pub async fn run_check(
        &self,
        ctx: &SecurityContext,
        tenant: Uuid,
        period: &str,
        check_type: &str,
    ) -> Result<Uuid, DomainError> {
        match check_type {
            CHECK_AR_DERIVED => self.check_ar_derived(ctx, tenant, period).await,
            CHECK_PAYMENTS_PSP => self
                .check_payments_psp(ctx, tenant, period)
                .await?
                .ok_or_else(|| {
                    DomainError::InvalidRequest(
                        "PSP settlement feed not configured for this period (check inert)"
                            .to_owned(),
                    )
                }),
            CHECK_INVOICE_COMPLETENESS => self
                .check_invoice_completeness(ctx, tenant, period)
                .await?
                .ok_or_else(|| {
                    DomainError::InvalidRequest(
                        "issued-invoice manifest not configured for this period (check inert)"
                            .to_owned(),
                    )
                }),
            other => Err(DomainError::InvalidRequest(format!(
                "unknown reconciliation check_type: {other}"
            ))),
        }
    }

    /// The near-real-time ticker pass (cadence from `ReconConfig.recon_tick_secs`):
    /// for every tenant with posted rows, reconcile its current OPEN period across all
    /// three checks. AR↔derived always runs; Payments↔PSP + invoice-completeness are
    /// inert until their control feeds land. A per-tenant failure is logged and skipped
    /// (one flaky tenant must not starve the rest); the recon defects themselves are
    /// reported via the runs / exceptions / alarms, not as `Err`.
    ///
    /// # Errors
    /// Returns `Err` only if the up-front tenant enumeration fails (DB unreachable).
    pub async fn run(&self) -> anyhow::Result<()> {
        let ctx = SecurityContext::anonymous();
        let tenant_ids = self.enumerate_tenants().await?;
        let mut failed = 0_usize;
        for tenant in tenant_ids {
            let scope = AccessScope::for_tenant(tenant);
            let period = match self.periods.current_open_period(&scope, tenant).await {
                Ok(Some(p)) => p,
                // No open period (tenant not provisioned / between periods) — nothing to
                // reconcile this tick.
                Ok(None) => continue,
                Err(e) => {
                    failed += 1;
                    tracing::warn!(target: "bss-ledger", %tenant, error = %e, "recon tick: open-period resolve failed; skipping tenant");
                    continue;
                }
            };
            // AR↔derived (always available). PSP + completeness are inert until configured.
            for check in [
                CHECK_AR_DERIVED,
                CHECK_PAYMENTS_PSP,
                CHECK_INVOICE_COMPLETENESS,
            ] {
                let result = match check {
                    CHECK_AR_DERIVED => {
                        self.check_ar_derived(&ctx, tenant, &period).await.map(Some)
                    }
                    CHECK_PAYMENTS_PSP => self.check_payments_psp(&ctx, tenant, &period).await,
                    _ => self.check_invoice_completeness(&ctx, tenant, &period).await,
                };
                if let Err(e) = result {
                    failed += 1;
                    tracing::warn!(target: "bss-ledger", %tenant, check, error = %e, "recon tick: check failed; continuing");
                }
            }
        }
        if failed > 0 {
            tracing::warn!(
                failed,
                "bss-ledger: reconciliation tick completed with per-tenant/check failures"
            );
        }
        Ok(())
    }

    /// Enumerate every tenant with posted rows (the same all-tenants `allow_all`
    /// enumeration the tie-out job uses).
    async fn enumerate_tenants(&self) -> anyhow::Result<HashSet<Uuid>> {
        #[derive(Debug, FromQueryResult)]
        struct TenantRow {
            tenant_id: Uuid,
        }
        let conn = self.db.conn()?;
        // Project to DISTINCT tenant_id rather than materializing every journal_entry
        // header — this recon tick only needs the SET of tenants with posted rows, and
        // journal_entry is the largest table in the gear. The scoped projection keeps
        // the all-tenants system scope applied (avoids the full-table `.all()` scan).
        let rows = journal_entry::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .project_all(&conn, |q| {
                q.select_only()
                    .column(journal_entry::Column::TenantId)
                    .distinct()
                    .into_model::<TenantRow>()
            })
            .await
            .map_err(|e| anyhow::anyhow!("recon: enumerate tenants: {e}"))?;
        Ok(rows.into_iter().map(|r| r.tenant_id).collect())
    }

    /// **H2 — AR↔derived (AC #7).** Wrap [`TieOutJob::tie_out_on`] as the `AR_DERIVED`
    /// run: the AR cache vs the journal-recomputed projection. Variance > tolerance
    /// (X4) → `RECON_MISMATCH` + `ReconciliationVariance` alarm + blocks close.
    async fn check_ar_derived(
        &self,
        ctx: &SecurityContext,
        tenant: Uuid,
        period: &str,
    ) -> Result<Uuid, DomainError> {
        let run_id = Uuid::now_v7();
        let scope = AccessScope::for_tenant(tenant);
        let publisher = Arc::clone(&self.publisher);
        let db = self.db.clone();
        let ctx = ctx.clone();
        let period_owned = period.to_owned();
        let per_k = self.config.ar_tolerance_minor_per_k_lines;

        let outcome: Result<ReconOutcome, DbError> = self
            .db
            .transaction(move |txn| {
                let scope = scope.clone();
                let publisher = Arc::clone(&publisher);
                let db = db.clone();
                let ctx = ctx.clone();
                let period_owned = period_owned.clone();
                Box::pin(async move {
                    ReconciliationRunRepo::start(
                        txn,
                        &scope,
                        tenant,
                        run_id,
                        &period_owned,
                        CHECK_AR_DERIVED,
                    )
                    .await
                    .map_err(repo_to_db)?;
                    // VHP-1843: prefer the incremental tie-out (baseline + fold of
                    // the open period) over the all-time full fold; fall back to the
                    // full fold when there is no baseline yet (the tenant has never
                    // closed a period) or a period is transitional. The incremental
                    // path advances `reconciliation_run.watermark` to its verified
                    // boundary; the full fallback leaves it unset.
                    let job = TieOutJob::new(db.clone(), Arc::clone(&publisher));
                    let (report, watermark) =
                        match job.tie_out_incremental(txn, tenant).await.map_err(|e| {
                            DbError::Other(anyhow::anyhow!("recon AR incremental tie-out: {e}"))
                        })? {
                            Some(inc) => {
                                let wm = inc.watermark;
                                (inc.into_tie_out_report(tenant), wm)
                            }
                            None => (
                                job.tie_out_on(txn, tenant).await.map_err(|e| {
                                    DbError::Other(anyhow::anyhow!("recon AR tie-out: {e}"))
                                })?,
                                None,
                            ),
                        };
                    let (variance_minor, within_tolerance) = ar_tolerance_eval(&report, per_k);
                    let detail = json!({
                        "summary": report.summary(),
                        "posted_line_count": report.posted_line_count,
                        "tolerance_minor_per_k_lines": per_k,
                    });
                    ReconciliationRunRepo::finalize(
                        txn,
                        &scope,
                        tenant,
                        run_id,
                        RUN_STATUS_DONE,
                        variance_minor,
                        within_tolerance,
                        watermark,
                        Some(detail),
                    )
                    .await
                    .map_err(repo_to_db)?;
                    Self::emit_completed_in_txn(
                        &publisher,
                        &ctx,
                        txn,
                        tenant,
                        run_id,
                        &period_owned,
                        CHECK_AR_DERIVED,
                        variance_minor,
                        within_tolerance,
                    )
                    .await?;
                    Ok(ReconOutcome {
                        variance_minor,
                        within_tolerance,
                    })
                })
            })
            .await;

        let outcome =
            outcome.map_err(|e| DomainError::Internal(format!("recon AR_DERIVED run: {e}")))?;
        self.record_and_route(
            tenant,
            period,
            CHECK_AR_DERIVED,
            ExceptionType::ReconMismatch,
            &outcome,
        )
        .await;
        Ok(run_id)
    }

    /// **H3 — Payments↔PSP.** Reconcile the ledger's recorded settlements against the
    /// PSP settlement report (the `PspSettlementFeedV1` control feed). `None` report ⇒
    /// inert (`Ok(None)`, no run). A divergence beyond tolerance → `PSP_VARIANCE` +
    /// alarm + blocks close. (The stuck-refund-clearing leg of the design's Payments↔PSP
    /// tie is owned by the aged-alarm job's `STUCK_REFUND_CLEARING` routing, Phase 2.)
    async fn check_payments_psp(
        &self,
        ctx: &SecurityContext,
        tenant: Uuid,
        period: &str,
    ) -> Result<Option<Uuid>, DomainError> {
        let report = self
            .psp_feed
            .settlement_report(tenant, period)
            .await
            .map_err(|e| DomainError::Internal(format!("recon PSP settlement feed: {e}")))?;
        let Some(psp) = report else {
            // Inert: no PSP report for the period (feed not configured / nothing pushed).
            return Ok(None);
        };

        let run_id = Uuid::now_v7();
        let scope = AccessScope::for_tenant(tenant);
        let publisher = Arc::clone(&self.publisher);
        let ctx = ctx.clone();
        let period_owned = period.to_owned();
        let psp_settled = psp.settled_minor;
        // PSP rounding tolerance rate (captured for the move-closure below).
        let per_k = i64::from(self.config.ar_tolerance_minor_per_k_lines);
        let db = self.db.clone();

        let outcome: Result<ReconOutcome, DbError> = self
            .db
            .transaction(move |txn| {
                let scope = scope.clone();
                let publisher = Arc::clone(&publisher);
                let ctx = ctx.clone();
                let period_owned = period_owned.clone();
                let db = db.clone();
                Box::pin(async move {
                    ReconciliationRunRepo::start(
                        txn,
                        &scope,
                        tenant,
                        run_id,
                        &period_owned,
                        CHECK_PAYMENTS_PSP,
                    )
                    .await
                    .map_err(repo_to_db)?;
                    // Ledger-side settled total, PERIOD-SCOPED (C2): the net-of-returns
                    // sum of this period's PAYMENT_SETTLE / SETTLEMENT_RETURN UNALLOCATED
                    // legs, on the SAME net basis as the PSP report. The prior lifetime
                    // `payment_settlement.settled_minor` sum (PK tenant+payment, NO period
                    // column) compared a tenant-lifetime total against a per-period PSP
                    // figure — a multi-period tenant diverged. Folds in i128, narrowed
                    // checked inside the helper (no `unwrap_or(i64::MAX)` saturation).
                    let (ledger_settled, settle_count) = JournalRepo::new(db.clone())
                        .sum_period_settled_net(txn, &scope, tenant, &period_owned)
                        .await
                        .map_err(repo_to_db)?;
                    // Store the variance MAGNITUDE in the shared `variance_minor` column
                    // (consistent with the AR check, which sums absolute divergences); the
                    // signed direction stays recoverable from the ledger/psp totals in `detail`.
                    let variance_minor = ledger_settled.saturating_sub(psp_settled).abs();
                    // Rounding tolerance, mirroring AR (X4): exact-match is brittle for
                    // cross-system penny rounding, so allow `per_k` minor units per 1,000
                    // settlements, floored at the statutory minimum (`per_k`). A divergence
                    // above the budget is a real variance and blocks close.
                    let budget = per_k
                        .saturating_mul(i64::try_from(settle_count / 1000).unwrap_or(i64::MAX))
                        .max(per_k);
                    let within_tolerance = variance_minor <= budget;
                    let detail = json!({
                        "ledger_settled_minor": ledger_settled,
                        "psp_settled_minor": psp_settled,
                        "psp_currency": psp.currency,
                        "psp_report_id": psp.report_id,
                    });
                    ReconciliationRunRepo::finalize(
                        txn,
                        &scope,
                        tenant,
                        run_id,
                        RUN_STATUS_DONE,
                        variance_minor,
                        within_tolerance,
                        None,
                        Some(detail),
                    )
                    .await
                    .map_err(repo_to_db)?;
                    Self::emit_completed_in_txn(
                        &publisher,
                        &ctx,
                        txn,
                        tenant,
                        run_id,
                        &period_owned,
                        CHECK_PAYMENTS_PSP,
                        variance_minor,
                        within_tolerance,
                    )
                    .await?;
                    Ok(ReconOutcome {
                        variance_minor,
                        within_tolerance,
                    })
                })
            })
            .await;

        let outcome =
            outcome.map_err(|e| DomainError::Internal(format!("recon PAYMENTS_PSP run: {e}")))?;
        self.record_and_route(
            tenant,
            period,
            CHECK_PAYMENTS_PSP,
            ExceptionType::PspVariance,
            &outcome,
        )
        .await;
        Ok(Some(run_id))
    }

    /// **I3 — invoice-completeness (N-recon-1).** Reconcile the independent
    /// issued-invoice manifest (`IssuedInvoiceManifestV1`) against the set of
    /// `INVOICE_POST` entries committed to the journal for `(tenant, period)`:
    /// `issued − posted`. `None` manifest ⇒ inert (`Ok(None)`, no run). Any issued
    /// `invoiceId` with no committed entry — or a count mismatch — opens a
    /// `MISSED_POSTING` exception per missing id (close-blocking) + the `MissedPosting`
    /// alarm. The close-blocking rows are gated by `manifest_enforcement` (default OFF
    /// until the feed is live, design decision 3); the run + the variance are always
    /// recorded (audit).
    async fn check_invoice_completeness(
        &self,
        ctx: &SecurityContext,
        tenant: Uuid,
        period: &str,
    ) -> Result<Option<Uuid>, DomainError> {
        let manifest = self
            .manifest_feed
            .latest_manifest(tenant, period)
            .await
            .map_err(|e| DomainError::Internal(format!("recon issued-invoice manifest: {e}")))?;
        let Some(manifest) = manifest else {
            // Inert: no manifest for the period (feed not configured / nothing pushed).
            return Ok(None);
        };

        let run_id = Uuid::now_v7();
        let scope = AccessScope::for_tenant(tenant);
        let publisher = Arc::clone(&self.publisher);
        let ctx = ctx.clone();
        let period_owned = period.to_owned();
        let issued: Vec<String> = manifest.invoice_ids.clone();
        let manifest_count = manifest.count;

        // The set of missing issued ids is computed inside the txn and carried out for
        // the exception routing below. (Default isolation — the authoritative gate is the
        // SERIALIZABLE close path; this run is an audit record, self-healing via the tick.)
        let (run_outcome, missing): (Result<ReconOutcome, DbError>, Vec<String>) = {
            let db_result = self
                .db
                .transaction({
                    let scope = scope.clone();
                    let publisher = Arc::clone(&publisher);
                    let ctx = ctx.clone();
                    let period_owned = period_owned.clone();
                    let issued = issued.clone();
                    move |txn| {
                        let scope = scope.clone();
                        let publisher = Arc::clone(&publisher);
                        let ctx = ctx.clone();
                        let period_owned = period_owned.clone();
                        let issued = issued.clone();
                        Box::pin(async move {
                            ReconciliationRunRepo::start(
                                txn,
                                &scope,
                                tenant,
                                run_id,
                                &period_owned,
                                CHECK_INVOICE_COMPLETENESS,
                            )
                            .await
                            .map_err(repo_to_db)?;
                            let posted = posted_invoice_ids(txn, &scope, tenant, &period_owned)
                                .await
                                .map_err(|e| {
                                    DbError::Other(anyhow::anyhow!(
                                        "recon completeness: read posted invoices: {e}"
                                    ))
                                })?;
                            let missing: Vec<String> = issued
                                .iter()
                                .filter(|id| !posted.contains(*id))
                                .cloned()
                                .collect();
                            let count_mismatch =
                                manifest_count != u64::try_from(posted.len()).unwrap_or(u64::MAX);
                            let within_tolerance = missing.is_empty() && !count_mismatch;
                            let variance_minor = i64::try_from(missing.len()).unwrap_or(i64::MAX);
                            let detail = json!({
                                "issued_count": manifest_count,
                                "posted_count": posted.len(),
                                "missing_count": missing.len(),
                                "count_mismatch": count_mismatch,
                            });
                            ReconciliationRunRepo::finalize(
                                txn,
                                &scope,
                                tenant,
                                run_id,
                                RUN_STATUS_DONE,
                                variance_minor,
                                within_tolerance,
                                None,
                                Some(detail),
                            )
                            .await
                            .map_err(repo_to_db)?;
                            Self::emit_completed_in_txn(
                                &publisher,
                                &ctx,
                                txn,
                                tenant,
                                run_id,
                                &period_owned,
                                CHECK_INVOICE_COMPLETENESS,
                                variance_minor,
                                within_tolerance,
                            )
                            .await?;
                            Ok((
                                ReconOutcome {
                                    variance_minor,
                                    within_tolerance,
                                },
                                missing,
                            ))
                        })
                    }
                })
                .await;
            match db_result {
                Ok((outcome, missing)) => (Ok(outcome), missing),
                Err(e) => (Err(e), Vec::new()),
            }
        };

        let outcome = run_outcome
            .map_err(|e| DomainError::Internal(format!("recon INVOICE_COMPLETENESS run: {e}")))?;

        self.metrics.reconciliation_run(CHECK_INVOICE_COMPLETENESS);
        self.metrics
            .reconciliation_variance_minor(CHECK_INVOICE_COMPLETENESS, outcome.variance_minor);
        // Clear any OPEN MISSED_POSTING whose invoice has since been posted (the §4.6
        // idempotent re-post). Runs regardless of tolerance — a now-complete period must
        // clear its stale close-blocking rows even when `within_tolerance` is true.
        self.resolve_landed_missed_postings(tenant, period, &issued, &missing)
            .await;
        if !outcome.within_tolerance {
            self.metrics
                .reconciliation_out_of_tolerance(CHECK_INVOICE_COMPLETENESS);
            // The close-blocking rows + the page are flag-gated: a missed posting only
            // blocks close (and pages) once `manifest_enforcement` is ON (the feed is
            // live). With it OFF the run + variance are still recorded (audit), but the
            // gap is inert (design decision 3 / §4.5 residual risk).
            if self.config.manifest_enforcement {
                for invoice_id in &missing {
                    self.exceptions
                        .route_for_period(
                            tenant,
                            ExceptionType::MissedPosting,
                            invoice_id,
                            period,
                            Some(json!({ "period_id": period, "check_type": CHECK_INVOICE_COMPLETENESS })),
                        )
                        .await;
                }
                self.emit_alarm(
                    tenant,
                    CHECK_INVOICE_COMPLETENESS,
                    AlarmCategory::MissedPosting,
                    outcome.variance_minor,
                )
                .await;
            }
        }
        Ok(Some(run_id))
    }

    /// Emit `billing.ledger.reconciliation.completed` in the run's transaction (so the
    /// event commits atomically with the `finalize` write).
    #[allow(
        clippy::too_many_arguments,
        reason = "the event carries the full run identity + result"
    )]
    async fn emit_completed_in_txn(
        publisher: &Arc<LedgerEventPublisher>,
        ctx: &SecurityContext,
        txn: &DbTx<'_>,
        tenant: Uuid,
        run_id: Uuid,
        period: &str,
        check_type: &str,
        variance_minor: i64,
        within_tolerance: bool,
    ) -> Result<(), DbError> {
        publisher
            .publish_reconciliation_completed(
                ctx,
                txn,
                LedgerReconciliationCompleted {
                    tenant_id: tenant,
                    run_id,
                    period_id: period.to_owned(),
                    check_type: check_type.to_owned(),
                    variance_minor,
                    within_tolerance,
                    at_utc: OffsetDateTime::now_utc(),
                },
            )
            .await
            .map_err(|e| DbError::Other(anyhow::anyhow!("publish reconciliation.completed: {e}")))
    }

    /// Record the run metrics and, on an out-of-tolerance result, route the
    /// close-blocking exception (fire-and-forget) + raise the `ReconciliationVariance`
    /// alarm. Used by the AR↔derived + Payments↔PSP checks (invoice-completeness routes
    /// per-missing-id behind the manifest flag, so it does its own out-of-band step).
    async fn record_and_route(
        &self,
        tenant: Uuid,
        period: &str,
        check_type: &str,
        ex_type: ExceptionType,
        outcome: &ReconOutcome,
    ) {
        self.metrics.reconciliation_run(check_type);
        self.metrics
            .reconciliation_variance_minor(check_type, outcome.variance_minor);
        if !outcome.within_tolerance {
            self.metrics.reconciliation_out_of_tolerance(check_type);
            let business_ref = format!("recon:{period}:{check_type}");
            self.exceptions
                .route_for_period(
                    tenant,
                    ex_type,
                    &business_ref,
                    period,
                    Some(json!({
                        "check_type": check_type,
                        "period_id": period,
                        "variance_minor": outcome.variance_minor,
                    })),
                )
                .await;
            self.emit_alarm(
                tenant,
                check_type,
                AlarmCategory::ReconciliationVariance,
                outcome.variance_minor,
            )
            .await;
        }
    }

    /// Raise a reconciliation alarm (fire-and-forget, out-of-band) with the catalog's
    /// severity for `category`.
    async fn emit_alarm(
        &self,
        tenant: Uuid,
        check_type: &str,
        category: AlarmCategory,
        variance_minor: i64,
    ) {
        let alarm = LedgerInvariantAlarm {
            category,
            severity: severity(category),
            tenant_id: tenant,
            scope: format!("tenant:{tenant}"),
            code: category.as_str().to_owned(),
            detail: format!("check={check_type} variance_minor={variance_minor}"),
            affected: Vec::new(),
        };
        self.publisher
            .emit_invariant_alarm(&SecurityContext::anonymous(), alarm)
            .await;
    }

    /// Resolve any OPEN `MISSED_POSTING` whose missing invoice has since been posted —
    /// `issued − missing` is the now-committed set (design §4.6 idempotent re-post).
    /// Fire-and-forget: a list/resolve failure is logged, never fails the run.
    async fn resolve_landed_missed_postings(
        &self,
        tenant: Uuid,
        period: &str,
        issued: &[String],
        missing: &[String],
    ) {
        let missing_set: HashSet<&str> = missing.iter().map(String::as_str).collect();
        let landed: HashSet<&str> = issued
            .iter()
            .map(String::as_str)
            .filter(|id| !missing_set.contains(id))
            .collect();
        if landed.is_empty() {
            return;
        }
        let scope = AccessScope::for_tenant(tenant);
        let open = match self.exception_repo.list(&scope, tenant, Some("OPEN")).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(target: "bss-ledger", %tenant, error = %e, "recon completeness: list OPEN exceptions for auto-resolve failed");
                return;
            }
        };
        for row in open.iter().filter(|r| {
            r.exception_type == ExceptionType::MissedPosting.as_str()
                && r.period_id.as_deref() == Some(period)
                && landed.contains(r.business_ref.as_str())
        }) {
            if let Err(e) = self
                .exception_repo
                .resolve_one(
                    &scope,
                    tenant,
                    row.exception_id,
                    "RESOLVED",
                    "system:invoice-completeness",
                )
                .await
            {
                tracing::warn!(target: "bss-ledger", %tenant, exception_id = %row.exception_id, error = %e, "recon completeness: auto-resolve MISSED_POSTING failed");
            }
        }
    }
}

/// Evaluate the AR↔derived tie-out report against the X4 rounding tolerance.
/// Returns `(variance_minor, within_tolerance)`.
///
/// A clean report ties out exactly (`0`, within). Otherwise the total **absolute**
/// monetary divergence (account-balance + sub-grain + payment-counter variances) is the
/// `variance_minor`, and it is within tolerance only when there is **no** structural
/// defect (an imbalanced entry / a negative guarded grain / a PENDING mapping line are
/// hard defects, never rounding) AND the divergence fits the rounding budget
/// `(posted_line_count / 1000) * per_k_lines` (X4: ≤ `per_k_lines` minor units per 1,000
/// posted lines; statutory floors override — floored at `per_k_lines` minor units).
fn ar_tolerance_eval(report: &TieOutReport, per_k_lines: u32) -> (i64, bool) {
    if report.is_clean() {
        return (0, true);
    }
    let total: i128 = report
        .account_balance_variances
        .iter()
        .map(|v| (i128::from(v.computed) - i128::from(v.cached)).abs())
        .chain(
            report
                .sub_grain_variances
                .iter()
                .map(|v| (i128::from(v.computed) - i128::from(v.cached)).abs()),
        )
        .chain(
            report
                .payment_counter_variances
                .iter()
                .map(|v| (i128::from(v.computed) - i128::from(v.cached)).abs()),
        )
        .sum();
    let variance_minor = i64::try_from(total).unwrap_or(i64::MAX);
    let hard_defect = !report.imbalanced_entries.is_empty()
        || !report.negative_grains.is_empty()
        || report.pending_lines > 0;
    // X4 per-1000-lines rounding allowance, FLOORED at a statutory minimum
    // (`per_k_lines` minor units) so a sub-1,000-line period can still absorb the
    // immaterial-rounding bucket the design grants ("statutory floors override") —
    // integer division alone yields 0 under 1,000 lines, blocking on 1 minor of
    // legitimate rounding. (A per-jurisdiction statutory registry remains future.)
    let budget = i64::from(per_k_lines)
        .saturating_mul(i64::try_from(report.posted_line_count / 1000).unwrap_or(i64::MAX))
        .max(i64::from(per_k_lines));
    let within_tolerance = !hard_defect && variance_minor <= budget;
    (variance_minor, within_tolerance)
}

/// The set of `INVOICE_POST` `source_business_id`s (invoiceIds) committed to the journal
/// for `(tenant, period)` — the "posted" side of the invoice-completeness set difference.
/// Shared with the close gate's pre-close completeness check (`infra::period_close`).
pub(crate) async fn posted_invoice_ids(
    txn: &DbTx<'_>,
    scope: &AccessScope,
    tenant: Uuid,
    period: &str,
) -> Result<HashSet<String>, DbError> {
    let entries = journal_entry::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(
            Condition::all()
                .add(journal_entry::Column::TenantId.eq(tenant))
                .add(journal_entry::Column::PeriodId.eq(period.to_owned()))
                .add(journal_entry::Column::SourceDocType.eq(SourceDocType::InvoicePost.as_str())),
        )
        .all(txn)
        .await
        .map_err(|e| DbError::Other(anyhow::anyhow!("read INVOICE_POST entries: {e}")))?;
    Ok(entries.into_iter().map(|e| e.source_business_id).collect())
}

#[cfg(test)]
#[path = "reconciliation_tests.rs"]
mod tests;
