//! `RevaluationRunJob` — the periodic Mode-B unrealized-revaluation ticker
//! (Slice 5 Phase 3, design §4.5 / plan I1). Each tick, for every provisioned
//! tenant, it:
//!
//! - **forward-revalues the current period at period end** — only on a tick that
//!   falls on the last UTC day of the period (so the resolved rate is the
//!   period-end rate and the period is still OPEN to post into). A mid-period
//!   tick does NOT forward-revalue (it would freeze a mid-period rate); the
//!   explicit `POST /fx/revaluation-runs` (I2) is the operator/ERP trigger for
//!   precise control / a missed period-end.
//! - **reverses the previous period every tick** — `reverse_period` is idempotent
//!   and self-deferring (it no-ops until that period has CLOSED and a later OPEN
//!   period exists), so running it every tick across the whole next month is a
//!   robust catch-up with a wide window (unlike the one-day forward window).
//!
//! ## Provider-agnostic + fail-safe (mirrors `RateSyncJob` / `PeriodOpenJob`)
//! Disabled (Mode A / unset `revaluation_enabled`) ⇒ the tick is a no-op. The
//! provisioned tenants come from the fiscal-calendar feed under the system-context
//! `allow_all`; a per-tenant failure (e.g. no period-end rate) is isolated
//! (logged, the pass continues) and NEVER aborts the gear. The actor is the
//! system-context [`SecurityContext::anonymous`].

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::FxConfig;
use crate::domain::fx::revaluation_mode::RevaluationMode;
use crate::domain::period::{period_id_for, previous_period_id};
use crate::domain::ports::metrics::LedgerMetricsPort;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::fx::revaluation_run::UnrealizedRevaluationRun;
use crate::infra::storage::repo::{FxRevaluationModeRepo, FxRevaluationRunRepo, ReferenceRepo};
use time::Duration;
use time::OffsetDateTime;

/// Outcome of one revaluation tick (returned for testability; the serve loop only
/// logs a tick error).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RevaluationRunReport {
    /// `false` when Mode-B is disabled (the tick was a no-op).
    pub enabled: bool,
    /// `true` when this tick fell on a period-end day (forward revaluation ran).
    pub forward_attempted: bool,
    /// Provisioned tenants processed this tick.
    pub tenants: u64,
    /// Tenants whose forward/reversal raised an isolated fault (logged, skipped).
    pub failed_tenants: u64,
}

/// Periodic Mode-B unrealized-revaluation job: forward-revalue at period end +
/// reverse the previous period every tick.
pub struct RevaluationRunJob {
    db: DBProvider<DbError>,
    publisher: Arc<LedgerEventPublisher>,
    metrics: Arc<dyn LedgerMetricsPort>,
    fx: FxConfig,
    /// Per-tenant FX revaluation mode (VHP-1986): resolves Mode A/B for each
    /// provisioned tenant each tick (an explicit row wins; else the fleet default
    /// from `fx.revaluation_enabled`).
    mode_repo: FxRevaluationModeRepo,
}

impl RevaluationRunJob {
    /// Build the job over one database provider (the provisioned-tenant
    /// enumeration + the runner), the event publisher (threaded into the posting
    /// engine), the metrics sink (the run-pass duration histogram), and the FX
    /// config (the Mode-B gate + rate source).
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
        fx: FxConfig,
    ) -> Self {
        let mode_repo = FxRevaluationModeRepo::new(db.clone());
        Self {
            db,
            publisher,
            metrics,
            fx,
            mode_repo,
        }
    }

    /// Run one revaluation pass across every provisioned tenant.
    ///
    /// # Errors
    /// Returns `Err` only on an infrastructure failure enumerating the
    /// provisioned tenants (the pass cannot start). A per-tenant forward/reversal
    /// fault is isolated within the pass.
    pub async fn run(&self) -> anyhow::Result<RevaluationRunReport> {
        // VHP-1986: no global early-return — the Mode A/B decision is per-tenant
        // (resolved in the loop below). The global `fx.revaluation_enabled` is only
        // the fleet default for tenants without an explicit per-tenant mode row.
        let started = Instant::now();
        let tenants = self.provisioned_tenants().await?;
        let runner = UnrealizedRevaluationRun::new(
            self.db.clone(),
            Arc::clone(&self.publisher),
            self.fx.clone(),
        )
        .with_metrics(Arc::clone(&self.metrics));
        let ctx = SecurityContext::anonymous();

        // Period-end detection: a tick is a period-end tick when tomorrow falls in
        // a different period (the last UTC day of the current period). Plain UTC
        // month arithmetic (decision 1).
        let now = OffsetDateTime::now_utc();
        let today = period_id_for(now);
        let is_period_end = period_id_for(now + Duration::days(1)) != today;
        let prev = previous_period_id(&today);

        let mut report = RevaluationRunReport {
            // The fleet default (VHP-1986): `fx.revaluation_enabled` is the default
            // for tenants WITHOUT an explicit mode row, NOT a hard global gate.
            enabled: self.fx.revaluation_enabled,
            forward_attempted: is_period_end,
            tenants: u64::try_from(tenants.len()).unwrap_or(u64::MAX),
            failed_tenants: 0,
        };
        for tenant in tenants {
            let scope = AccessScope::for_tenant(tenant);
            // VHP-1986 per-tenant Mode A/B: an explicit row wins; else the global
            // `fx.revaluation_enabled` is the fleet default (on→ModeB, off→ModeA).
            let mode = match self
                .mode_repo
                .read_effective_mode(&scope, tenant, now)
                .await
            {
                Ok(stored) => {
                    stored.unwrap_or(RevaluationMode::fleet_default(self.fx.revaluation_enabled))
                }
                Err(e) => {
                    report.failed_tenants += 1;
                    tracing::error!(
                        tenant_id = %tenant,
                        error = %e,
                        "bss-ledger: revaluation mode read failed for tenant; skipping"
                    );
                    continue;
                }
            };
            if let Err(e) = self
                .process_tenant(
                    &runner,
                    &ctx,
                    &scope,
                    tenant,
                    &today,
                    prev.as_deref(),
                    is_period_end,
                    mode.revalues(),
                )
                .await
            {
                report.failed_tenants += 1;
                tracing::error!(
                    tenant_id = %tenant,
                    error = %e,
                    "bss-ledger: revaluation tick failed for tenant; continuing"
                );
            }
        }
        if report.failed_tenants > 0 {
            tracing::warn!(
                failed_tenants = report.failed_tenants,
                tenants = report.tenants,
                "bss-ledger: revaluation tick completed with per-tenant failures"
            );
        }
        self.metrics
            .fx_revaluation_duration(started.elapsed().as_secs_f64());
        Ok(report)
    }

    /// Forward-revalue the current period (period-end ticks only) and reverse the
    /// previous period (every tick). A whole tenant is one isolation unit.
    #[allow(clippy::too_many_arguments)]
    async fn process_tenant(
        &self,
        runner: &UnrealizedRevaluationRun,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        today: &str,
        prev: Option<&str>,
        is_period_end: bool,
        revalue: bool,
    ) -> anyhow::Result<()> {
        // Forward revaluation runs only for a current Mode-B tenant (VHP-1986).
        if is_period_end && revalue {
            runner
                .run_period(ctx, scope, tenant, today, true)
                .await
                .map_err(|e| anyhow::anyhow!("forward revaluation {today}: {e}"))?;
            // C3: record the period-end revaluation COMPLETE so the period-close
            // gate can REQUIRE it (a failed/lagged run leaves no marker → close
            // blocks + alarms, instead of certifying a period whose missing
            // FX_REVALUATION the closed-period guard makes unpostable forever). The
            // run's posts already committed; this is an out-of-band idempotent
            // upsert on a scoped connection, so a retried tick refreshes it.
            let conn = self
                .db
                .conn()
                .map_err(|e| anyhow::anyhow!("revaluation marker conn: {e}"))?;
            FxRevaluationRunRepo::mark_complete(&conn, scope, tenant, today)
                .await
                .map_err(|e| anyhow::anyhow!("mark revaluation complete {today}: {e}"))?;
        }
        // Reversal is always attempted (idempotent + self-deferring): it reverses a
        // prior Mode-B period even if the tenant has since switched to Mode A, and
        // no-ops (NothingToReverse) for a tenant that never revalued.
        if let Some(prev) = prev {
            runner
                .reverse_period(ctx, scope, tenant, prev, true)
                .await
                .map_err(|e| anyhow::anyhow!("reversal {prev}: {e}"))?;
        }
        Ok(())
    }

    /// Enumerate the distinct provisioned tenants (the fiscal-calendar feed under
    /// the system-context `allow_all`, deduped).
    ///
    /// # Errors
    /// Returns `Err` on an infrastructure failure reading the calendar feed.
    async fn provisioned_tenants(&self) -> anyhow::Result<BTreeSet<Uuid>> {
        let repo = ReferenceRepo::new(self.db.clone());
        let calendars = repo
            .list_all_fiscal_calendars()
            .await
            .map_err(|e| anyhow::anyhow!("revaluation: enumerate provisioned tenants: {e}"))?;
        Ok(calendars.into_iter().map(|c| c.tenant_id).collect())
    }
}
