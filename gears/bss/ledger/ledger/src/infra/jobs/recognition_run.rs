//! `RecognitionRunJob` — the periodic ASC 606 S6 **release** ticker (Slice 4,
//! Group F2). Each tick it finds the tenants with due `PENDING`/`QUEUED`
//! recognition segments ACROSS ALL TENANTS and triggers one recognition run per
//! tenant **for the CURRENT period**, releasing that period's due segments
//! (the current period + any past-due catch-up; FUTURE segments wait until their
//! period is current — never recognized early, H1/Z6-1) through the
//! [`RecognitionRunService`] (the same orchestration the `POST
//! /recognition-runs` REST endpoint drives).
//!
//! The REST endpoint is the on-demand trigger; this job is the automatic
//! backstop so a due period's segments release promptly once the period opens
//! even with no operator call — exactly as the `QueueApplierJob` backstops the
//! drain-on-settle hook. Both are idempotent: a run releases each segment
//! at-most-once via the per-segment `RECOGNITION` idempotency gate (a re-run
//! re-credits nothing), so a redundant tick is harmless.
//!
//! ## System-context / cross-tenant (mirrors `QueueApplierJob` / `period_open`)
//! Finding due work is an UNSCOPED, cross-tenant read
//! ([`RecognitionRepo::list_due_tenant_periods`] under
//! [`AccessScope::allow_all`], the sanctioned system-context pattern). Each run
//! is then SCOPED per-tenant by [`AccessScope::for_tenant`] when the
//! [`RecognitionRunService`] posts — the cross-tenant list is only a candidate
//! feed. A per-`(tenant, period)` run error is isolated (logged, the pass
//! continues) so one flaky tenant doesn't starve the rest, exactly like the
//! peer jobs. The run actor is the system-context [`SecurityContext::anonymous`]
//! (the same anonymous actor the queue-applier sweep posts under — this is not a
//! per-request caller).

use std::sync::Arc;

use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::ports::metrics::LedgerMetricsPort;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::recognition::run_service::RecognitionRunService;
use crate::infra::storage::repo::RecognitionRepo;
use time::OffsetDateTime;

/// Upper bound on the cross-tenant due-segment candidate scan per tick — a
/// ceiling on how many `PENDING` segment rows one pass loads to derive the
/// distinct `(tenant, period)` work set (mirrors the sweep job's
/// `SWEEP_DISCOVERY_LIMIT`). A backlog beyond this drains over subsequent ticks
/// (each run is idempotent), so a pathological queue can't load an unbounded
/// candidate set into memory.
const DISCOVERY_LIMIT: u64 = 10_000;

/// Outcome of one recognition-run tick (aggregated across every `(tenant,
/// period)` triggered).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecognitionRunReport {
    /// Distinct tenants with due work in/before the current period that were run
    /// this tick (each run targets the current period).
    pub pairs: u64,
    /// Pairs whose run completed (ran-in-order or queued-out-of-order).
    pub triggered: u64,
    /// Pairs whose run raised an error (isolated; logged + skipped).
    pub failed: u64,
}

/// Periodic recognition-run job over every tenant/period with due `PENDING`
/// recognition segments.
pub struct RecognitionRunJob {
    db: DBProvider<DbError>,
    publisher: Arc<LedgerEventPublisher>,
    metrics: Arc<dyn LedgerMetricsPort>,
}

impl RecognitionRunJob {
    /// Build the job over one database provider, the event publisher (threaded
    /// into the run-service's posting engine + the recognition alarms), and the
    /// metrics sink (the §9 recognition metrics). Mirrors
    /// [`crate::infra::jobs::queue_applier::QueueApplierJob::new`].
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
    ) -> Self {
        Self {
            db,
            publisher,
            metrics,
        }
    }

    /// Trigger a recognition run for every `(tenant, period)` with due `PENDING`
    /// work. A per-pair run error is isolated (logged, the pass continues) so one
    /// flaky tenant doesn't abort the tick — mirrors the queue-applier / tie-out
    /// jobs.
    ///
    /// # Errors
    /// Returns `Err` only on an infrastructure failure reading the cross-tenant
    /// due-work feed (the pass cannot start); per-pair run faults are swallowed
    /// within the pass.
    pub async fn run(&self) -> anyhow::Result<RecognitionRunReport> {
        let repo = RecognitionRepo::new(self.db.clone());
        let pairs = repo
            .list_due_tenant_periods(DISCOVERY_LIMIT)
            .await
            .map_err(|e| anyhow::anyhow!("recognition-run: enumerate due tenant/periods: {e}"))?;

        // One run-service for the whole pass (same db/publisher/metrics clones as
        // the REST surface's in-process client builds).
        let svc = RecognitionRunService::new(
            self.db.clone(),
            Arc::clone(&self.publisher),
            Arc::clone(&self.metrics),
        );
        // System-context actor for the released posts (the tick is not a
        // per-request actor; the entry header's actor is stamped from this ctx).
        let ctx = SecurityContext::anonymous();

        // The CURRENT fiscal period (wall-clock month). A run TARGETS this period:
        // `run_period` releases the due segments (`period_id <= current` — the
        // current period + any past-due catch-up via the E-2 missed-close
        // reassignment) and naturally EXCLUDES future segments. Triggering a run per
        // distinct pending period instead (incl. the NEXT period `PeriodOpenJob`
        // pre-opens) would recognize next month's segment THIS month — an ASC 606
        // early-recognition break (H1/Z6-1). A tenant whose only due work is a
        // future period has nothing to release yet and is skipped this tick.
        let current = crate::domain::period::period_id_for(OffsetDateTime::now_utc());
        let tenants: std::collections::BTreeSet<Uuid> = pairs
            .iter()
            .filter(|(_, period_id)| period_id.as_str() <= current.as_str())
            .map(|(tenant, _)| *tenant)
            .collect();

        let tenant_count = u64::try_from(tenants.len()).unwrap_or(u64::MAX);
        let mut report = RecognitionRunReport::default();
        for tenant in tenants {
            // Per-tenant scope: the run posts into the seller's own ledger (the
            // cross-tenant feed above is only a candidate set), targeting the
            // CURRENT period. A fresh `run_id` is minted per tick (None) — the
            // ticker is the backstop, not an idempotency-keyed retry; per-segment
            // at-most-once makes a re-run safe regardless.
            let scope = AccessScope::for_tenant(tenant);
            match svc.trigger(&ctx, &scope, tenant, &current, None).await {
                Ok(_) => report.triggered += 1,
                Err(e) => {
                    // Isolate per-tenant faults: log and continue so one flaky
                    // tenant doesn't starve the rest of the tick.
                    report.failed += 1;
                    tracing::error!(
                        tenant_id = %tenant,
                        period_id = %current,
                        error = %e,
                        "bss-ledger: recognition-run trigger failed for tenant; continuing"
                    );
                }
            }
        }
        report.pairs = tenant_count;
        if report.failed > 0 {
            tracing::warn!(
                failed = report.failed,
                pairs = report.pairs,
                "bss-ledger: recognition-run tick completed with per-pair failures"
            );
        }
        Ok(report)
    }
}

#[cfg(test)]
#[path = "recognition_run_tests.rs"]
mod tests;
