//! ToolKit declaration, dependency wiring, and lifecycle for [`BssLedgerGear`].
//!
//! Initialization runs migrations, builds the ledger services, registers
//! `dyn LedgerClientV1` in `ClientHub`, and prepares the REST state. The
//! stateful lifecycle drives reconciliation and maintenance jobs under a
//! cancellation token and shuts them down cooperatively.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use axum::Router;
use bss_ledger_sdk::api::LedgerClientV1;
use bss_ledger_sdk::{
    BillRunFinishedV1, IssuedInvoiceManifestV1, PspSettlementFeedV1, RateProviderV1,
    UnconfiguredRateProviderV1,
};
use sea_orm_migration::{MigrationTrait, MigratorTrait};
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::config::ConfigError;
use toolkit::contracts::{DatabaseCapability, RestApiCapability};
use toolkit::{Gear, GearCtx};
use toolkit_db::{DBProvider, DbError};
use tracing::{info, warn};

use crate::api::local_client::LedgerLocalClient;
use crate::config::BssLedgerConfig;
use crate::domain::ports::metrics::LedgerMetricsPort;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::metrics::LedgerMetricsMeter;
use crate::infra::storage::migrations::Migrator;

/// Typed bundle of the per-process state that `init()` builds and the
/// lifecycle reads: `register_rest()` reads `provisioning`; `serve()` reads
/// `db`, `publisher`, and the three tick cadences to drive the background jobs.
pub(crate) struct LedgerRuntime {
    /// REST provisioning state (the provisioning route reuses the same
    /// in-process client built in `init()`).
    pub provisioning: Arc<crate::api::rest::provisioning::ApiState>,
    /// REST journal-entry / balance state: the same in-process client (reads)
    /// plus the `InvoicePostService` write orchestrator (post / reversal /
    /// mapping-correction), built once in `init()`.
    pub journal: Arc<crate::api::rest::journal_entries::ApiState>,
    /// REST payment state (settle / allocate / list-allocations /
    /// read-unallocated): the same in-process client, which gates the PEP and
    /// orchestrates the money-in / money-out posts.
    pub payments: Arc<crate::api::rest::payments::ApiState>,
    /// REST credit-application state (grant / apply reusable credit): the same
    /// in-process client, which gates the PEP and orchestrates the wallet posts.
    pub credit: Arc<crate::api::rest::credit::ApiState>,
    /// REST chargeback dispute state (record a dispute phase): the same
    /// in-process client, which gates the PEP and orchestrates the dispute posts.
    pub disputes: Arc<crate::api::rest::disputes::ApiState>,
    /// REST recognition-run state (trigger an ASC 606 S6 release for a period):
    /// the same in-process client, which gates the PEP and orchestrates the run.
    pub recognition: Arc<crate::api::rest::recognition::ApiState>,
    /// REST adjustment state (Slice 3): the concrete credit-note / debit-note
    /// orchestrators + the adjustment repo behind `POST /credit-notes`,
    /// `POST /debit-notes`, and `GET /invoices/{id}/exposure`.
    pub adjustments: Arc<crate::api::rest::adjustments::ApiState>,
    /// REST dual-control approval-queue state (VHP-1852): the `ApprovalService`
    /// lifecycle engine behind the approve / reject / rework / comment surface.
    pub approvals: Arc<crate::api::rest::approvals::ApiState>,
    /// REST tenant invoice-posting-policy state (VHP-1853): the missing-mapping
    /// mode + AR-aging bucket thresholds write / read surface.
    pub posting_policy: Arc<crate::api::rest::posting_policy::ApiState>,
    /// REST per-tenant FX revaluation-mode state (VHP-1986): the Mode A/B
    /// write / read config surface.
    pub fx_revaluation_mode: Arc<crate::api::rest::fx_revaluation_mode::ApiState>,
    /// The dual-control engine itself (Group G): threaded into the queue-applier
    /// sweep so the refund de-quarantine drain gates over the THEN-CURRENT D2
    /// threshold (it never auto-posts an over-threshold released refund).
    pub approval: Arc<crate::infra::approval::service::ApprovalService>,
    /// REST refund state (Slice 3 Group G): the gated `RefundHandler` (+ composite
    /// credit-note handler) + the adjustment repo behind `POST /refunds`,
    /// `POST /refund-with-credit-note`, and `GET /refunds/{refundId}`.
    pub refunds: Arc<crate::api::rest::refunds::ApiState>,
    /// REST payer-closure state (VHP-1852 Phase 2): the dual-control engine + the
    /// payer lifecycle repo behind `POST /payers/{id}/close`.
    pub payers: Arc<crate::api::rest::payers::ApiState>,
    /// REST fiscal-period-closure state (Slice 7 Group C): the in-process client
    /// (gated close + `period.closed` emit) + the dual-control engine (reopen)
    /// behind `POST /legal-entities/{le}/periods/{period}/closure`.
    pub closure: Arc<crate::api::rest::closure::ApiState>,
    /// REST exception-queue state (Slice 7 Phase 2): the scoped queue repo behind
    /// `GET /exceptions` + `POST /exceptions/{id}/resolution`.
    pub exceptions: Arc<crate::api::rest::exceptions::ApiState>,
    /// The exception router (Slice 7 Phase 2): held so the `serve()` aged-alarm job
    /// can route `STUCK_REFUND_CLEARING` into the durable close-blocking queue.
    pub exception_router: Arc<crate::infra::exception::ExceptionRouter>,
    /// REST reconciliation state (Slice 7 Phase 3): the `ReconciliationFramework`
    /// (the `POST /reconciliation-runs` trigger — also driven by the `serve()` ticker
    /// via `reconciliation.framework`) + the run read repo (`GET /reconciliation-runs/{id}`).
    pub reconciliation: Arc<crate::api::rest::reconciliation::ApiState>,
    /// REST control-feed ingest state (Slice 7 Phase 3): the in-process control store
    /// the `POST …/control/*` endpoints push into (shared with the framework + the
    /// close gate's pre-close manifest / bill-run reads).
    pub control: Arc<crate::api::rest::control::ApiState>,
    /// REST audit-retrieval state (Group 2C): the scoped audit reader + the
    /// cross-tenant elevation gateway, built once in `init()`.
    pub audit: Arc<crate::api::rest::audit::ApiState>,
    /// REST FX state (Slice 5): the FX rate store behind `POST /fx/rates` (the
    /// secondary seed ingest) + `GET /fx/rate-snapshots/{id}` (immutable snapshot
    /// read).
    pub fx: Arc<crate::api::rest::fx::ApiState>,
    /// Platform PEP, built in `init()` from the `authz-resolver` `ClientHub`
    /// client and cloned into every request as an `Extension` by
    /// `register_rest`; also threaded into the in-process client. Authz is
    /// security-critical, so a missing client fails init (no no-op fallback).
    pub enforcer: Arc<authz_resolver_sdk::PolicyEnforcer>,
    /// Database provider for the system-context background jobs (cross-tenant
    /// raw SQL / unscoped reads — NOT the per-request `SecureORM` scope).
    pub db: DBProvider<DbError>,
    /// Event publisher, used out-of-band by the tie-out job to emit invariant
    /// alarms on a separate connection.
    pub publisher: Arc<LedgerEventPublisher>,
    /// Process-global `OTel` metrics handle (`infra::metrics`), built in `init()`
    /// and shared by the in-process client, the publisher's alarm mirror, and the
    /// queued-allocation sweep (the queue-depth gauge + the apply posts).
    pub metrics: Arc<dyn LedgerMetricsPort>,
    /// Tie-out tick cadence.
    pub tie_out_tick: Duration,
    /// How often the daily tie-out folds the full all-time history instead of the
    /// incremental (baseline + open-period) path — every `N`th tick (VHP-1843).
    pub tie_out_full_every_n: u64,
    /// Fiscal-period-open tick cadence.
    pub period_open_tick: Duration,
    /// Queued-allocation sweep tick cadence (the deferred-apply backstop).
    pub queue_applier_tick: Duration,
    /// Aged-alarm tick cadence (the §6 `Warn`-severity aged-work scan).
    pub aged_alarm_tick: Duration,
    /// Recognition-run tick cadence (the Slice 4 §4.3 S6 release backstop).
    pub recognition_run_tick: Duration,
    /// Chain-verifier tick cadence.
    pub verify_tick: Duration,
    /// Process `ClientHub` handle used to LAZILY discover the FX rate-provider
    /// plugin each rate-sync tick (the composite the external `bss-rate-provider`
    /// core gear registers as a `RateProviderPluginSpecV1` instance). Resolving
    /// per tick — rather than once at init — means a late-registered adapter is
    /// picked up on the next tick and there is no init-ordering dependency; absent
    /// an adapter the resolver yields the fail-safe `UnconfiguredRateProviderV1`.
    pub rate_provider_hub: Arc<toolkit::client_hub::ClientHub>,
    /// Vendor selecting which rate-provider plugin instance the ticker resolves.
    pub rate_provider_vendor: String,
    /// FX rate store (the `RateSyncJob` upsert target / the lock-time
    /// `RateSource` read target).
    pub fx_repo: crate::infra::storage::repo::FxRepo,
    /// FX rate-sync tick cadence.
    pub fx_rate_sync_tick: Duration,
    /// FX config (the Mode-B `revaluation_enabled` gate + rate source) for the
    /// `RevaluationRunJob`.
    pub fx_config: crate::config::FxConfig,
    /// Payments config (the per-allocation touched-invoice cap) — threaded into
    /// the `QueueApplierJob` so the deferred-apply drain honours the same cap as
    /// the inline allocate path.
    pub payments_config: crate::config::PaymentsConfig,
    /// Unrealized-revaluation tick cadence (Slice 5 Phase 3 Mode-B run).
    pub revaluation_run_tick: Duration,
    /// Reconciliation-job tick cadence (Slice 7 Phase 3 near-real-time recon pass:
    /// invoice-completeness + AR↔derived + Payments↔PSP).
    pub recon_tick: Duration,
}

// NOTE: `event-broker` is intentionally NOT a declared dep — the event layer is
// parked (no `event-broker-sdk` in this repo; `build_event_publisher` builds a
// broker-free no-op/metrics-only publisher). Declaring it would make the gear
// unschedulable (the orchestrator can't satisfy a dep no gear provides). Re-add
// it here once the broker gear exists and `events_enabled` is wired to it.
//
// NOTE: the FX rate-provider adapter is NOT a `deps` edge — the `RateSyncJob`
// discovers it lazily from the types-registry each tick (see `resolve_rate_provider`),
// so a late-registered adapter self-heals and the ledger stays decoupled from
// (and schedulable without) the external `bss-rate-provider` gear.
#[toolkit::gear(name = "bss-ledger", capabilities = [db, rest, stateful], deps = [types_registry, authz_resolver, account_management], lifecycle(entry = "serve", stop_timeout = "30s"))]
pub struct BssLedgerGear {
    /// Typed runtime built inside [`Gear::init`] and consumed by
    /// [`RestApiCapability::register_rest`] and [`BssLedgerGear::serve`].
    /// `None` when `init()` has not completed or short-circuited on a
    /// default-disabled boot.
    runtime: ArcSwapOption<LedgerRuntime>,
}

impl Default for BssLedgerGear {
    fn default() -> Self {
        Self {
            runtime: ArcSwapOption::from(None),
        }
    }
}

/// Discover the FX rate-provider plugin from the types-registry (the composite
/// the external `bss-rate-provider` core gear registers), selecting the instance
/// by `vendor` + `priority`. Called fresh each rate-sync tick, so a
/// late-registered adapter is picked up on the next tick — no init-ordering
/// dependency. Falls back to the fail-safe
/// [`UnconfiguredRateProviderV1`](bss_ledger_sdk::UnconfiguredRateProviderV1)
/// whenever the registry is unavailable or no plugin is registered, so a missing
/// adapter is a benign deployment state (the sync job logs at debug, no alarm),
/// never a books defect.
async fn resolve_rate_provider(
    hub: &toolkit::client_hub::ClientHub,
    vendor: &str,
) -> Arc<dyn RateProviderV1> {
    let fallback = || Arc::new(UnconfiguredRateProviderV1) as Arc<dyn RateProviderV1>;
    // Every branch below logs before falling back. Silently returning the
    // fail-safe made a types-registry outage or a malformed plugin instance
    // indistinguishable from the benign "no adapter configured" state, so FX
    // rates would go stale with nothing to diagnose from. Levels split by
    // meaning: `warn` for something broken, `debug` for the expected
    // not-configured case (the sync job already reports that one).
    let registry = match hub.get::<dyn types_registry_sdk::TypesRegistryClient>() {
        Ok(registry) => registry,
        Err(e) => {
            tracing::warn!(
                vendor,
                error = %e,
                "bss-ledger: types-registry client unavailable; using the fail-safe FX \
                 rate provider (rates will go stale)"
            );
            return fallback();
        }
    };
    let type_id = bss_ledger_sdk::RateProviderPluginSpecV1::gts_type_id();
    let instances = match registry
        .list_instances(
            types_registry_sdk::InstanceQuery::new().with_pattern(format!("{type_id}*")),
        )
        .await
    {
        Ok(instances) => instances,
        Err(e) => {
            tracing::warn!(
                vendor,
                error = %e,
                "bss-ledger: listing FX rate-provider plugin instances failed; using the \
                 fail-safe FX rate provider (rates will go stale)"
            );
            return fallback();
        }
    };
    match toolkit::plugins::choose_plugin_instance::<bss_ledger_sdk::RateProviderPluginSpecV1>(
        vendor,
        instances.iter().map(|e| (e.id.as_ref(), &e.object)),
    ) {
        Ok(gts_id) => hub
            .try_get_scoped::<dyn RateProviderV1>(&toolkit::client_hub::ClientScope::gts_id(
                &gts_id,
            ))
            .unwrap_or_else(|| {
                // Partial registration: the plugin published its instance but
                // never registered (or failed before registering) its scoped
                // client. Not the benign unconfigured state — warn.
                tracing::warn!(
                    vendor,
                    instance_id = %gts_id,
                    "bss-ledger: FX rate-provider plugin instance has no scoped client \
                     registered; using the fail-safe FX rate provider"
                );
                fallback()
            }),
        Err(e) => {
            tracing::debug!(
                vendor,
                error = %e,
                "bss-ledger: no FX rate-provider plugin instance matched; using the \
                 fail-safe default"
            );
            fallback()
        }
    }
}

impl BssLedgerGear {
    /// Lifecycle entry (`stateful` capability). Spawns the daily tie-out,
    /// fiscal-period-open, and chain-verifier tickers under a child of `cancel`.
    /// A tick failure is logged and the loop continues (a transient job error
    /// must not kill the gear); a panicking task cancels the others so the
    /// runtime sees an abort.
    ///
    /// No `await_ready`: job readiness must NOT gate request traffic, so the
    /// signature omits the `ReadySignal` argument.
    ///
    /// # Errors
    /// Returns `Err` only if a spawned ticker task panics / aborts (surfaced
    /// as a join error); cooperative cancel-token shutdown returns `Ok(())`.
    /// Spawn the recognition-run ticker (the Slice 4 S6 release backstop): a
    /// cancellable loop that drives a `RecognitionRunJob` pass every
    /// `recognition_run_tick`. Kept out-of-line (its shape mirrors the peer
    /// tickers) so `serve` stays within the line budget.
    fn spawn_recognition_ticker(
        rt: Arc<LedgerRuntime>,
        token: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(rt.recognition_run_tick);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    biased;
                    () = token.cancelled() => break,
                    _ = iv.tick() => {
                        let job = crate::infra::jobs::recognition_run::RecognitionRunJob::new(
                            rt.db.clone(),
                            Arc::clone(&rt.publisher),
                            Arc::clone(&rt.metrics),
                        );
                        if let Err(e) = job.run().await {
                            tracing::error!(error = %e, "bss-ledger: recognition-run job tick failed");
                        }
                    }
                }
            }
        })
    }

    /// Spawn the FX rate-sync ticker (Slice 5 §4.6): a cancellable loop that
    /// drives a `RateSyncJob` pass every `fx_rate_sync_tick`, pulling the
    /// configured `RateProviderV1` plugin's latest rates into the local store.
    /// Extracted (its shape mirrors the peer tickers) so `serve` stays within the
    /// line budget. With the default `UnconfiguredRateProviderV1` the pass is inert
    /// (logs at debug + returns, no alarm).
    fn spawn_rate_sync_ticker(
        rt: Arc<LedgerRuntime>,
        token: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(rt.fx_rate_sync_tick);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    biased;
                    () = token.cancelled() => break,
                    _ = iv.tick() => {
                        // Heartbeat FIRST, before discovery: this must count the tick
                        // itself. A stalled ticker attempts no fetch, so it produces no
                        // fetch error and never raises FX_SNAPSHOT_MISSING — the rate
                        // store just ages until a post blocks on staleness. Incrementing
                        // after resolve/fetch would go flat on a discovery failure too,
                        // making a dead ticker indistinguishable from an empty registry.
                        rt.metrics.fx_rate_sync_ticked();
                        // Timed from here, alongside the heartbeat, so the sample covers
                        // the whole pass — discovery included. The composite tries its
                        // sources sequentially, so the pass, not any single source's
                        // fetch, is what has to fit inside `fx_rate_sync_tick`; nothing
                        // else measures it. The loop awaits the pass inline, so a pass
                        // that outgrows the interval cannot overlap the next one — it
                        // delays it, and this histogram is where that shows up first.
                        let started = std::time::Instant::now();
                        // Discover the rate-provider plugin fresh each tick (lazy,
                        // self-healing) — the fail-safe default when none is registered.
                        let provider = resolve_rate_provider(
                            &rt.rate_provider_hub,
                            &rt.rate_provider_vendor,
                        )
                        .await;
                        let job = crate::infra::jobs::rate_sync::RateSyncJob::new(
                            rt.db.clone(),
                            provider,
                            rt.fx_repo.clone(),
                            Arc::clone(&rt.publisher),
                        );
                        if let Err(e) = job.run().await {
                            tracing::error!(error = %e, "bss-ledger: rate-sync job tick failed");
                        }
                        // Recorded on the failure path too: a pass that fails slowly is
                        // exactly the one worth seeing, and dropping those samples would
                        // bias the p95 the alert reads.
                        rt.metrics.fx_rate_sync_duration(started.elapsed().as_secs_f64());
                    }
                }
            }
        })
    }

    /// Spawn the unrealized-revaluation ticker (Slice 5 Phase 3, design §4.5): a
    /// cancellable loop that drives a `RevaluationRunJob` pass every
    /// `revaluation_run_tick` — forward-revalues at period end + reverses the
    /// previous period. With Mode-B disabled (`revaluation_enabled = false`) the
    /// pass is a no-op. Extracted (its shape mirrors the peer tickers) so `serve`
    /// stays within the line budget.
    fn spawn_revaluation_ticker(
        rt: Arc<LedgerRuntime>,
        token: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(rt.revaluation_run_tick);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    biased;
                    () = token.cancelled() => break,
                    _ = iv.tick() => {
                        let job = crate::infra::jobs::revaluation_run::RevaluationRunJob::new(
                            rt.db.clone(),
                            Arc::clone(&rt.publisher),
                            Arc::clone(&rt.metrics),
                            rt.fx_config.clone(),
                        );
                        if let Err(e) = job.run().await {
                            tracing::error!(error = %e, "bss-ledger: revaluation-run job tick failed");
                        }
                    }
                }
            }
        })
    }

    /// Spawn the reconciliation ticker (Slice 7 Phase 3, design §4.3): a cancellable
    /// loop that drives a `ReconciliationFramework::run()` pass every `recon_tick` —
    /// the near-real-time reconciliation sweep (invoice-completeness + AR↔derived +
    /// Payments↔PSP over each tenant's current OPEN period). The Payments↔PSP +
    /// invoice-completeness checks are inert until their control feeds land. Extracted
    /// (its shape mirrors the peer tickers) so `serve` stays within the line budget.
    fn spawn_reconciliation_ticker(
        rt: Arc<LedgerRuntime>,
        token: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(rt.recon_tick);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    biased;
                    () = token.cancelled() => break,
                    _ = iv.tick() => {
                        if let Err(e) = rt.reconciliation.framework.run().await {
                            tracing::error!(error = %e, "bss-ledger: reconciliation job tick failed");
                        }
                    }
                }
            }
        })
    }

    #[allow(
        clippy::redundant_pub_crate,
        reason = "gear-private serve entry-point invoked by the toolkit runtime"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "the serve loop declares one ticker + one select arm per background job (tie-out, period-open, queue-applier, aged-alarm, recognition-run, rate-sync, revaluation-run, reconciliation, chain-verify); the per-job wiring is flat by design"
    )]
    pub(crate) async fn serve(self: Arc<Self>, cancel: CancellationToken) -> anyhow::Result<()> {
        let Some(rt) = self.runtime.load_full() else {
            info!("bss-ledger: serve() with no runtime (unconfigured); idling until cancelled");
            cancel.cancelled().await;
            return Ok(());
        };

        // Shared child token — cancelled by either the runtime (normal
        // shutdown via `cancel`) or by `serve()` itself when one ticker dies,
        // so both tickers observe the same cancellation deterministically.
        let tasks = cancel.child_token();

        // Tie-out ticker. The first `interval` tick fires immediately — a
        // startup tie-out is harmless and idempotent.
        let mut tie = {
            let rt = Arc::clone(&rt);
            let c = tasks.clone();
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(rt.tie_out_tick);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // VHP-1843: most ticks take the incremental tie-out (baseline + open
                // fold); every Nth tick (and the first after startup) folds the full
                // all-time history as a drift backstop. `N <= 1` → always full.
                let full_every_n = rt.tie_out_full_every_n;
                let mut tick_count: u64 = 0;
                loop {
                    tokio::select! {
                        biased;
                        () = c.cancelled() => break,
                        _ = iv.tick() => {
                            tick_count = tick_count.wrapping_add(1);
                            let full = full_every_n <= 1 || tick_count % full_every_n == 1;
                            let job = crate::infra::jobs::tieout::TieOutJob::new(
                                rt.db.clone(),
                                Arc::clone(&rt.publisher),
                            );
                            if let Err(e) = job.run_tick(full).await {
                                tracing::error!(error = %e, "bss-ledger: tie-out job tick failed");
                            }
                        }
                    }
                }
            })
        };

        // Period-open ticker (same shape).
        let mut period = {
            let rt = Arc::clone(&rt);
            let c = tasks.clone();
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(rt.period_open_tick);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        biased;
                        () = c.cancelled() => break,
                        _ = iv.tick() => {
                            let job = crate::infra::jobs::period_open::PeriodOpenJob::new(
                                rt.db.clone(),
                            );
                            if let Err(e) = job.run().await {
                                tracing::error!(error = %e, "bss-ledger: period-open job tick failed");
                            }
                        }
                    }
                }
            })
        };

        // Queued-allocation sweep ticker (same shape) — the deferred-apply
        // backstop (Group D). Needs the publisher + metrics (the apply posts
        // through the engine and the sweep emits the queue-depth gauge), unlike
        // the other two jobs.
        let mut queue = {
            let rt = Arc::clone(&rt);
            let c = tasks.clone();
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(rt.queue_applier_tick);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        biased;
                        () = c.cancelled() => break,
                        _ = iv.tick() => {
                            let job = crate::infra::jobs::queue_applier::QueueApplierJob::new(
                                rt.db.clone(),
                                Arc::clone(&rt.publisher),
                                Arc::clone(&rt.metrics),
                            )
                            .with_max_invoices_per_allocation(
                                rt.payments_config.max_invoices_per_allocation,
                            )
                            // Group G: gate the de-quarantine drain over the THEN-CURRENT D2.
                            .with_approval(Arc::clone(&rt.approval));
                            if let Err(e) = job.run().await {
                                tracing::error!(error = %e, "bss-ledger: queue-applier job tick failed");
                            }
                        }
                    }
                }
            })
        };

        // Aged-alarm ticker (same shape) — the §6 Warn-severity scan for queued
        // work / parked unallocated cash that has aged past a threshold. Needs the
        // publisher (it emits the aged alarms out-of-band), like the tie-out job;
        // no metrics sink (unlike the queue sweep's depth gauge).
        let mut aged = {
            let rt = Arc::clone(&rt);
            let c = tasks.clone();
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(rt.aged_alarm_tick);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        biased;
                        () = c.cancelled() => break,
                        _ = iv.tick() => {
                            let job = crate::infra::jobs::aged_alarms::AgedAlarmJob::new(
                                rt.db.clone(),
                                Arc::clone(&rt.publisher),
                            )
                            .with_metrics(Arc::clone(&rt.metrics))
                            .with_exceptions(Arc::clone(&rt.exception_router));
                            if let Err(e) = job.run().await {
                                tracing::error!(error = %e, "bss-ledger: aged-alarm job tick failed");
                            }
                            // Dual-control maintenance (VHP-1852), on the aged-alarm
                            // cadence: the DC12 TTL sweep + the Z8-1 stuck-APPROVING
                            // probe. Best-effort — faults are logged inside, never
                            // fatal to the tick.
                            dual_control_maintenance_tick(rt.db.clone(), &rt.metrics).await;
                        }
                    }
                }
            })
        };

        // Recognition-run ticker (Slice 4 S6 release backstop) — extracted to
        // `spawn_recognition_ticker` (needs the publisher + metrics, like the
        // queue sweep: the run posts through the engine + emits the §9 metrics).
        let mut recognition = Self::spawn_recognition_ticker(Arc::clone(&rt), tasks.clone());
        // FX rate-sync ticker (Slice 5 §4.6) — extracted to `spawn_rate_sync_ticker`
        // (pulls the configured `RateProviderV1` plugin's latest rates into the
        // local store; inert under the default `UnconfiguredRateProviderV1`).
        let mut rate_sync = Self::spawn_rate_sync_ticker(Arc::clone(&rt), tasks.clone());
        // Unrealized-revaluation ticker (Slice 5 Phase 3) — extracted to
        // `spawn_revaluation_ticker` (forward-revalues at period end + reverses
        // the previous period; a no-op under Mode A / `revaluation_enabled =
        // false`).
        let mut revaluation = Self::spawn_revaluation_ticker(Arc::clone(&rt), tasks.clone());
        // Reconciliation ticker (Slice 7 Phase 3) — extracted to
        // `spawn_reconciliation_ticker` (the near-real-time recon sweep:
        // invoice-completeness + AR↔derived + Payments↔PSP per tenant's open period;
        // the control-feed checks are inert until their feeds land).
        let mut recon = Self::spawn_reconciliation_ticker(Arc::clone(&rt), tasks.clone());
        // Chain-verifier ticker (same shape): re-walks every tenant's
        // tamper-evidence chain and freezes + alarms a tenant whose chain no
        // longer verifies.
        let mut verify = {
            let rt = Arc::clone(&rt);
            let c = tasks.clone();
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(rt.verify_tick);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        biased;
                        () = c.cancelled() => break,
                        _ = iv.tick() => {
                            let job = crate::infra::jobs::verifier::ChainVerifierJob::new(
                                rt.db.clone(),
                                Arc::clone(&rt.publisher),
                                Arc::clone(&rt.metrics),
                            );
                            if let Err(e) = job.run().await {
                                tracing::error!(error = %e, "bss-ledger: chain-verify job tick failed");
                            }
                        }
                    }
                }
            })
        };

        info!(
            tie_out_tick_secs = rt.tie_out_tick.as_secs(),
            period_open_tick_secs = rt.period_open_tick.as_secs(),
            queue_applier_tick_secs = rt.queue_applier_tick.as_secs(),
            aged_alarm_tick_secs = rt.aged_alarm_tick.as_secs(),
            recognition_run_tick_secs = rt.recognition_run_tick.as_secs(),
            verify_tick_secs = rt.verify_tick.as_secs(),
            fx_rate_sync_tick_secs = rt.fx_rate_sync_tick.as_secs(),
            revaluation_run_tick_secs = rt.revaluation_run_tick.as_secs(),
            recon_tick_secs = rt.recon_tick.as_secs(),
            "bss-ledger background ticks started"
        );

        // `select!` on the join handles (not `join!`): a panic in one ticker
        // would otherwise stay invisible for up to a full tick. Each arm
        // cancels the shared token, awaits the survivors, and maps a join
        // error (panic / abort) to `anyhow`.
        let serve_result: anyhow::Result<()> = tokio::select! {
            res = &mut tie => {
                tasks.cancel();
                let period_res = (&mut period).await;
                let queue_res = (&mut queue).await;
                let aged_res = (&mut aged).await;
                let recognition_res = (&mut recognition).await;
                let rate_sync_res = (&mut rate_sync).await;
                let revaluation_res = (&mut revaluation).await;
                let verify_res = (&mut verify).await;
                let recon_res = (&mut recon).await;
                res.map_err(|e| anyhow::anyhow!("bss-ledger: tie-out task: {e}"))?;
                period_res.map_err(|e| anyhow::anyhow!("bss-ledger: period-open task: {e}"))?;
                queue_res.map_err(|e| anyhow::anyhow!("bss-ledger: queue-applier task: {e}"))?;
                aged_res.map_err(|e| anyhow::anyhow!("bss-ledger: aged-alarm task: {e}"))?;
                recognition_res.map_err(|e| anyhow::anyhow!("bss-ledger: recognition-run task: {e}"))?;
                rate_sync_res.map_err(|e| anyhow::anyhow!("bss-ledger: rate-sync task: {e}"))?;
                revaluation_res.map_err(|e| anyhow::anyhow!("bss-ledger: revaluation-run task: {e}"))?;
                verify_res.map_err(|e| anyhow::anyhow!("bss-ledger: chain-verify task: {e}"))?;
                recon_res.map_err(|e| anyhow::anyhow!("bss-ledger: reconciliation task: {e}"))?;
                Ok(())
            }
            res = &mut period => {
                tasks.cancel();
                let tie_res = (&mut tie).await;
                let queue_res = (&mut queue).await;
                let aged_res = (&mut aged).await;
                let recognition_res = (&mut recognition).await;
                let rate_sync_res = (&mut rate_sync).await;
                let revaluation_res = (&mut revaluation).await;
                let verify_res = (&mut verify).await;
                let recon_res = (&mut recon).await;
                res.map_err(|e| anyhow::anyhow!("bss-ledger: period-open task: {e}"))?;
                tie_res.map_err(|e| anyhow::anyhow!("bss-ledger: tie-out task: {e}"))?;
                queue_res.map_err(|e| anyhow::anyhow!("bss-ledger: queue-applier task: {e}"))?;
                aged_res.map_err(|e| anyhow::anyhow!("bss-ledger: aged-alarm task: {e}"))?;
                recognition_res.map_err(|e| anyhow::anyhow!("bss-ledger: recognition-run task: {e}"))?;
                rate_sync_res.map_err(|e| anyhow::anyhow!("bss-ledger: rate-sync task: {e}"))?;
                revaluation_res.map_err(|e| anyhow::anyhow!("bss-ledger: revaluation-run task: {e}"))?;
                verify_res.map_err(|e| anyhow::anyhow!("bss-ledger: chain-verify task: {e}"))?;
                recon_res.map_err(|e| anyhow::anyhow!("bss-ledger: reconciliation task: {e}"))?;
                Ok(())
            }
            res = &mut queue => {
                tasks.cancel();
                let tie_res = (&mut tie).await;
                let period_res = (&mut period).await;
                let aged_res = (&mut aged).await;
                let recognition_res = (&mut recognition).await;
                let rate_sync_res = (&mut rate_sync).await;
                let revaluation_res = (&mut revaluation).await;
                let verify_res = (&mut verify).await;
                let recon_res = (&mut recon).await;
                res.map_err(|e| anyhow::anyhow!("bss-ledger: queue-applier task: {e}"))?;
                tie_res.map_err(|e| anyhow::anyhow!("bss-ledger: tie-out task: {e}"))?;
                period_res.map_err(|e| anyhow::anyhow!("bss-ledger: period-open task: {e}"))?;
                aged_res.map_err(|e| anyhow::anyhow!("bss-ledger: aged-alarm task: {e}"))?;
                recognition_res.map_err(|e| anyhow::anyhow!("bss-ledger: recognition-run task: {e}"))?;
                rate_sync_res.map_err(|e| anyhow::anyhow!("bss-ledger: rate-sync task: {e}"))?;
                revaluation_res.map_err(|e| anyhow::anyhow!("bss-ledger: revaluation-run task: {e}"))?;
                verify_res.map_err(|e| anyhow::anyhow!("bss-ledger: chain-verify task: {e}"))?;
                recon_res.map_err(|e| anyhow::anyhow!("bss-ledger: reconciliation task: {e}"))?;
                Ok(())
            }
            res = &mut aged => {
                tasks.cancel();
                let tie_res = (&mut tie).await;
                let period_res = (&mut period).await;
                let queue_res = (&mut queue).await;
                let recognition_res = (&mut recognition).await;
                let rate_sync_res = (&mut rate_sync).await;
                let revaluation_res = (&mut revaluation).await;
                let verify_res = (&mut verify).await;
                let recon_res = (&mut recon).await;
                res.map_err(|e| anyhow::anyhow!("bss-ledger: aged-alarm task: {e}"))?;
                tie_res.map_err(|e| anyhow::anyhow!("bss-ledger: tie-out task: {e}"))?;
                period_res.map_err(|e| anyhow::anyhow!("bss-ledger: period-open task: {e}"))?;
                queue_res.map_err(|e| anyhow::anyhow!("bss-ledger: queue-applier task: {e}"))?;
                recognition_res.map_err(|e| anyhow::anyhow!("bss-ledger: recognition-run task: {e}"))?;
                rate_sync_res.map_err(|e| anyhow::anyhow!("bss-ledger: rate-sync task: {e}"))?;
                revaluation_res.map_err(|e| anyhow::anyhow!("bss-ledger: revaluation-run task: {e}"))?;
                verify_res.map_err(|e| anyhow::anyhow!("bss-ledger: chain-verify task: {e}"))?;
                recon_res.map_err(|e| anyhow::anyhow!("bss-ledger: reconciliation task: {e}"))?;
                Ok(())
            }
            res = &mut recognition => {
                tasks.cancel();
                let tie_res = (&mut tie).await;
                let period_res = (&mut period).await;
                let queue_res = (&mut queue).await;
                let aged_res = (&mut aged).await;
                let rate_sync_res = (&mut rate_sync).await;
                let revaluation_res = (&mut revaluation).await;
                let verify_res = (&mut verify).await;
                let recon_res = (&mut recon).await;
                res.map_err(|e| anyhow::anyhow!("bss-ledger: recognition-run task: {e}"))?;
                tie_res.map_err(|e| anyhow::anyhow!("bss-ledger: tie-out task: {e}"))?;
                period_res.map_err(|e| anyhow::anyhow!("bss-ledger: period-open task: {e}"))?;
                queue_res.map_err(|e| anyhow::anyhow!("bss-ledger: queue-applier task: {e}"))?;
                aged_res.map_err(|e| anyhow::anyhow!("bss-ledger: aged-alarm task: {e}"))?;
                rate_sync_res.map_err(|e| anyhow::anyhow!("bss-ledger: rate-sync task: {e}"))?;
                revaluation_res.map_err(|e| anyhow::anyhow!("bss-ledger: revaluation-run task: {e}"))?;
                verify_res.map_err(|e| anyhow::anyhow!("bss-ledger: chain-verify task: {e}"))?;
                recon_res.map_err(|e| anyhow::anyhow!("bss-ledger: reconciliation task: {e}"))?;
                Ok(())
            }
            res = &mut rate_sync => {
                tasks.cancel();
                let tie_res = (&mut tie).await;
                let period_res = (&mut period).await;
                let queue_res = (&mut queue).await;
                let aged_res = (&mut aged).await;
                let recognition_res = (&mut recognition).await;
                let revaluation_res = (&mut revaluation).await;
                let verify_res = (&mut verify).await;
                let recon_res = (&mut recon).await;
                res.map_err(|e| anyhow::anyhow!("bss-ledger: rate-sync task: {e}"))?;
                tie_res.map_err(|e| anyhow::anyhow!("bss-ledger: tie-out task: {e}"))?;
                period_res.map_err(|e| anyhow::anyhow!("bss-ledger: period-open task: {e}"))?;
                queue_res.map_err(|e| anyhow::anyhow!("bss-ledger: queue-applier task: {e}"))?;
                aged_res.map_err(|e| anyhow::anyhow!("bss-ledger: aged-alarm task: {e}"))?;
                recognition_res.map_err(|e| anyhow::anyhow!("bss-ledger: recognition-run task: {e}"))?;
                revaluation_res.map_err(|e| anyhow::anyhow!("bss-ledger: revaluation-run task: {e}"))?;
                verify_res.map_err(|e| anyhow::anyhow!("bss-ledger: chain-verify task: {e}"))?;
                recon_res.map_err(|e| anyhow::anyhow!("bss-ledger: reconciliation task: {e}"))?;
                Ok(())
            }
            res = &mut revaluation => {
                tasks.cancel();
                let tie_res = (&mut tie).await;
                let period_res = (&mut period).await;
                let queue_res = (&mut queue).await;
                let aged_res = (&mut aged).await;
                let recognition_res = (&mut recognition).await;
                let rate_sync_res = (&mut rate_sync).await;
                let verify_res = (&mut verify).await;
                let recon_res = (&mut recon).await;
                res.map_err(|e| anyhow::anyhow!("bss-ledger: revaluation-run task: {e}"))?;
                tie_res.map_err(|e| anyhow::anyhow!("bss-ledger: tie-out task: {e}"))?;
                period_res.map_err(|e| anyhow::anyhow!("bss-ledger: period-open task: {e}"))?;
                queue_res.map_err(|e| anyhow::anyhow!("bss-ledger: queue-applier task: {e}"))?;
                aged_res.map_err(|e| anyhow::anyhow!("bss-ledger: aged-alarm task: {e}"))?;
                recognition_res.map_err(|e| anyhow::anyhow!("bss-ledger: recognition-run task: {e}"))?;
                rate_sync_res.map_err(|e| anyhow::anyhow!("bss-ledger: rate-sync task: {e}"))?;
                verify_res.map_err(|e| anyhow::anyhow!("bss-ledger: chain-verify task: {e}"))?;
                recon_res.map_err(|e| anyhow::anyhow!("bss-ledger: reconciliation task: {e}"))?;
                Ok(())
            }
            res = &mut verify => {
                tasks.cancel();
                let tie_res = (&mut tie).await;
                let period_res = (&mut period).await;
                let queue_res = (&mut queue).await;
                let aged_res = (&mut aged).await;
                let recognition_res = (&mut recognition).await;
                let rate_sync_res = (&mut rate_sync).await;
                let revaluation_res = (&mut revaluation).await;
                let recon_res = (&mut recon).await;
                res.map_err(|e| anyhow::anyhow!("bss-ledger: chain-verify task: {e}"))?;
                tie_res.map_err(|e| anyhow::anyhow!("bss-ledger: tie-out task: {e}"))?;
                period_res.map_err(|e| anyhow::anyhow!("bss-ledger: period-open task: {e}"))?;
                queue_res.map_err(|e| anyhow::anyhow!("bss-ledger: queue-applier task: {e}"))?;
                aged_res.map_err(|e| anyhow::anyhow!("bss-ledger: aged-alarm task: {e}"))?;
                recognition_res.map_err(|e| anyhow::anyhow!("bss-ledger: recognition-run task: {e}"))?;
                rate_sync_res.map_err(|e| anyhow::anyhow!("bss-ledger: rate-sync task: {e}"))?;
                revaluation_res.map_err(|e| anyhow::anyhow!("bss-ledger: revaluation-run task: {e}"))?;
                recon_res.map_err(|e| anyhow::anyhow!("bss-ledger: reconciliation task: {e}"))?;
                Ok(())
            }
            res = &mut recon => {
                tasks.cancel();
                let tie_res = (&mut tie).await;
                let period_res = (&mut period).await;
                let queue_res = (&mut queue).await;
                let aged_res = (&mut aged).await;
                let recognition_res = (&mut recognition).await;
                let rate_sync_res = (&mut rate_sync).await;
                let revaluation_res = (&mut revaluation).await;
                let verify_res = (&mut verify).await;
                res.map_err(|e| anyhow::anyhow!("bss-ledger: reconciliation task: {e}"))?;
                tie_res.map_err(|e| anyhow::anyhow!("bss-ledger: tie-out task: {e}"))?;
                period_res.map_err(|e| anyhow::anyhow!("bss-ledger: period-open task: {e}"))?;
                queue_res.map_err(|e| anyhow::anyhow!("bss-ledger: queue-applier task: {e}"))?;
                aged_res.map_err(|e| anyhow::anyhow!("bss-ledger: aged-alarm task: {e}"))?;
                recognition_res.map_err(|e| anyhow::anyhow!("bss-ledger: recognition-run task: {e}"))?;
                rate_sync_res.map_err(|e| anyhow::anyhow!("bss-ledger: rate-sync task: {e}"))?;
                revaluation_res.map_err(|e| anyhow::anyhow!("bss-ledger: revaluation-run task: {e}"))?;
                verify_res.map_err(|e| anyhow::anyhow!("bss-ledger: chain-verify task: {e}"))?;
                Ok(())
            }
            () = cancel.cancelled() => {
                tasks.cancel();
                Ok(())
            }
        };
        info!("bss-ledger background ticks cancelled");
        serve_result
    }
}

/// One dual-control maintenance tick (VHP-1852), run on the aged-alarm cadence:
/// the DC12 TTL sweep (flip stale `PENDING`/`NEEDS_REWORK` approvals past their
/// `expires_at` to `EXPIRED`, cross-tenant `allow_all`, complementing the lazy
/// expire-on-read in `create_pending`) plus the Z8-1 stuck-`APPROVING` probe
/// (record the `ledger_dual_control_approving` gauge + warn when crash-stranded
/// latches linger). Best-effort: every fault is logged, never propagated.
async fn dual_control_maintenance_tick(
    db: DBProvider<DbError>,
    metrics: &Arc<dyn LedgerMetricsPort>,
) {
    let approvals = crate::infra::storage::repo::ApprovalRepo::new(db);
    match approvals.expire_due_all(chrono::Utc::now()).await {
        Ok(n) if n > 0 => tracing::info!(
            expired = n,
            "bss-ledger: dual-control TTL sweep expired stale approvals"
        ),
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "bss-ledger: dual-control TTL sweep failed"),
    }
    // A healthy approve clears the APPROVING latch (PENDING→APPROVING→APPROVED)
    // within one txn, so a count that stays > 0 across ticks is a crash-stranded
    // approve — excluded from the TTL sweep above, still holding the active-
    // uniqueness slot, recoverable only by a manual re-approve.
    match approvals.count_approving_all().await {
        Ok(n) => {
            metrics.dual_control_approving(i64::try_from(n).unwrap_or(i64::MAX));
            if n > 0 {
                tracing::warn!(
                    approving = n,
                    "bss-ledger: approvals in the APPROVING latch (sustained > 0 = a \
                     crash-stranded approve needing manual re-approve)"
                );
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "bss-ledger: dual-control APPROVING-latch probe failed");
        }
    }
}

#[async_trait]
impl Gear for BssLedgerGear {
    /// Publish the in-process `LedgerClientV1` when the gear is
    /// configured. Absent from `gears:` → no-op (the module is compiled in
    /// but unconfigured); present-but-invalid config aborts init loudly.
    #[allow(clippy::too_many_lines)] // composition root: one flat wiring sequence is clearer than helpers
    async fn init(&self, ctx: &GearCtx) -> Result<()> {
        match ctx.config::<BssLedgerConfig>() {
            // Configured, or present with no `config:` section (defaults only).
            Ok(_) | Err(ConfigError::MissingConfigSection { .. }) => {}
            Err(ConfigError::GearNotFound { .. }) => {
                info!(
                    "bss-ledger: not present in the `gears:` config block, \
                     skipping init() (module compiled in but unconfigured)"
                );
                return Ok(());
            }
            Err(e) => return Err(e).context("bss-ledger: invalid `bss-ledger` config section"),
        }

        // Bind the config after the control-flow match above: the `Ok` and
        // `MissingConfigSection` arms both fall through here, so
        // `unwrap_or_default()` yields the parsed config or the all-defaults
        // config respectively (the `GearNotFound` / parse-error arms already
        // returned). Abort init loudly on an invalid or incomplete config: a
        // zero job cadence would panic `tokio::time::interval` in `serve`, and
        // an absent `seller_tenant_types` would turn every provisioning request
        // into a "not a ledger owner" rejection. Note this makes a gear entry
        // with no `config:` block a startup error, not a defaults-only run.
        let cfg: BssLedgerConfig = ctx.config().unwrap_or_default();
        // One call: `BssLedgerConfig::validate` covers its own required fields
        // (`seller_tenant_types`) and delegates to every sub-config, so a
        // sub-config added later cannot be left unvalidated here by omission.
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("bss-ledger: invalid config: {e}"))?;

        let db = ctx.db_required().context(
            "bss-ledger: ctx.db_required() failed; the `db` capability is declared \
             but no DbHandle is available",
        )?;

        // OTel metrics handle bound to the process-global meter provider (a
        // no-op until the host wires an exporter). Built before the publisher so
        // the alarm-counter mirror shares the same instruments as the post path.
        let metrics: Arc<dyn LedgerMetricsPort> = Arc::new(LedgerMetricsMeter::from_global());

        // Build the event publisher with graceful degradation: any absence or
        // error of the broker / types-registry / schema-registration yields a
        // no-op publisher (events disabled). A financial post must never fail
        // because the events surface is misconfigured. The metrics handle backs
        // the alarm counter mirror even when the broker is absent.
        let publisher = Arc::new(
            build_event_publisher(ctx, &db, cfg.events_enabled, Arc::clone(&metrics)).await,
        );

        // Platform PEP. Unlike events (graceful no-op), authz is
        // security-critical: a ledger must not run unauthorized, so a missing
        // `AuthZResolverApi` fails init loudly. No `with_capabilities` —
        // the PDP degrades subtree predicates to a flat `In` (decision A).
        let authz_client = ctx
            .client_hub()
            .get::<dyn authz_resolver_sdk::AuthZResolverApi>()
            .context(
                "bss-ledger: AuthZResolverApi absent from ClientHub; \
                 authz-resolver module must be registered",
            )?;
        let enforcer = Arc::new(authz_resolver_sdk::PolicyEnforcer::new(authz_client));

        // Register the authz-label stub schemas so RBAC role-defs referencing
        // the ledger labels pass target-type validation. Mandatory (hard-fail),
        // unlike the graceful event-schema registration in
        // `build_event_publisher`.
        let registry = ctx
            .client_hub()
            .get::<dyn types_registry_sdk::TypesRegistryClient>()
            .context(
                "bss-ledger: TypesRegistryClient absent from ClientHub; \
                 types-registry module must be registered",
            )?;
        let results = registry
            .register(crate::authz::authz_label_type_schemas())
            .await
            .context("bss-ledger: register authz label schemas")?;
        for r in results {
            if let types_registry_sdk::RegisterResult::Err { gts_id, error } = r {
                anyhow::bail!(
                    "bss-ledger: failed to register authz label {}: {error}",
                    gts_id.as_deref().unwrap_or("?")
                );
            }
        }

        // Capture clones for the `LedgerRuntime` BEFORE `db` / `publisher` /
        // `enforcer` / `metrics` are moved into the client: `db` is moved into
        // `LedgerLocalClient::new`, and a clone of `publisher` goes
        // to the client while the original is stored for the serve loop; the
        // enforcer + metrics clones are stored for `register_rest` / the runtime.
        let jobs_db = db.clone();
        let approval_db = db.clone();
        let payer_state_db = db.clone();
        // The dual-control executor's un-gated refund orchestrator (Group D replay)
        // — cloned BEFORE `db`/`publisher` are moved into the in-process client.
        let refund_db = db.clone();
        let refund_publisher = Arc::clone(&publisher);
        // The dual-control executor's un-gated manual-adjustment orchestrator (Group 5
        // / Phase 3 replay) — same db/publisher deps as the refund replay handler,
        // cloned BEFORE the client move below.
        let manual_db = db.clone();
        let manual_publisher = Arc::clone(&publisher);
        // The dual-control executor's period-reopen replay (Slice 7) — its own
        // db/publisher clones, captured before the client move below.
        let period_close_db = db.clone();
        let period_close_publisher = Arc::clone(&publisher);
        // The Group-G GATED refund REST handler (`POST /refunds` +
        // `refund-with-credit-note` + `GET /refunds/{id}`) needs its OWN db/publisher
        // clones + a repo db clone — captured before the client move below.
        let db_for_refunds = db.clone();
        let publisher_for_refunds = Arc::clone(&publisher);
        let db_for_refund_repo = db.clone();
        // The dispute read-surface repo (`GET /disputes` list + `GET /disputes/{id}`
        // by-id) — a plain scoped read over its own db clone, captured before the
        // client move below (mirrors `db_for_refund_repo`).
        let db_for_dispute_repo = db.clone();
        // The journal entry-HEADER read-surface repo (R5: `GET /journal-entries` list)
        // — a plain scoped read over its own db clone, called DIRECTLY from the handler
        // (the by-id `GET /journal-entries/{id}` still goes through the client),
        // captured before the client move below (mirrors `db_for_dispute_repo`).
        let db_for_journal_repo = db.clone();
        let db_for_posting_policy = db.clone();
        let db_for_posting_policy_rest = db.clone();
        let db_for_fx_revaluation_mode = db.clone();
        let db_for_fx_revaluation_mode_rest = db.clone();
        // The recognition-run read-surface repo (R4: `GET /recognition-runs` list +
        // `GET /recognition-runs/{run_id}` by-id) and the payment-settlement read
        // repo (R4: `GET /payments/{payment_id}/settlement`) — plain scoped reads
        // over their own db clones, captured before the client move below (mirror
        // `db_for_dispute_repo`).
        let db_for_recognition_repo = db.clone();
        let db_for_payment_repo = db.clone();
        // Slice 7 Phase 2: the exception router (additive close-blocking routing,
        // shared across the module-built stub-bearing services + the aged-alarm job)
        // and the exception-queue dashboard repo. Captured before the client move.
        let db_for_exceptions = db.clone();
        let exception_router = crate::infra::exception::ExceptionRouter::shared(db.clone());
        // The Group-6 / Phase-3 GATED manual-adjustment REST handler
        // (`POST /manual-adjustments`) needs its OWN db/publisher clones — a SEPARATE
        // instance from the executor's un-gated `approval_manual_handler` (which
        // replays an already-approved adjustment and must never re-gate), mirroring the
        // refund surface's gated/un-gated split. Captured before the client move below.
        let manual_rest_db = db.clone();
        let manual_rest_publisher = Arc::clone(&publisher);
        // The Slice-3 GATED credit-note + debit-note REST handlers
        // (`POST …/credit-notes`, `POST …/debit-notes`) each need their OWN db/publisher
        // clones — SEPARATE instances from the un-gated `credit_note_handler` /
        // `debit_note_handler` built below (which back the refund composite's
        // `with_credit_note_handler` + the executor's `post_*_approved` replay, neither
        // of which gates), mirroring the refund/manual gated/un-gated split. The debit
        // note also re-runs the recognition derivation, so its gated instance takes a
        // clone of the same validated `cfg.recognition`. Captured before the client move
        // below.
        let credit_rest_db = db.clone();
        let credit_rest_publisher = Arc::clone(&publisher);
        let debit_rest_db = db.clone();
        let debit_rest_publisher = Arc::clone(&publisher);
        let jobs_publisher = Arc::clone(&publisher);
        let rt_enforcer = Arc::clone(&enforcer);
        let rt_metrics = Arc::clone(&metrics);

        // FX rate-provider plugin (Slice 5): discovered LAZILY from the
        // types-registry each rate-sync tick (see `resolve_rate_provider`), not
        // resolved once here — so a late-registered adapter is picked up next tick
        // and the ledger needs no `deps` edge on it. Absent an adapter the resolver
        // yields the fail-safe `UnconfiguredRateProviderV1` (every fetch errors →
        // the local store stays empty → FX-needing posts block at lock time, never
        // a silent wrong rate); a missing adapter is NOT fatal to init. The hub
        // handle + selection vendor are captured for the serve-loop ticker. The
        // `fx_repo` db clone is captured before `db` moves into the client below.
        let rate_provider_hub = ctx.client_hub();
        let rate_provider_vendor = cfg.fx.provider_vendor.clone();
        let fx_repo = crate::infra::storage::repo::FxRepo::new(db.clone());

        // Slice 7 Phase 3: the three launch-blocking control feeds (issued-invoice
        // manifest, bill-run-finished, PSP settlement). Each resolves from `ClientHub`
        // like the FX rate provider, with the IN-PROCESS store as the fail-safe default —
        // the `…/control/*` REST endpoints push into it, the `ReconciliationFramework` +
        // the close gate read it back. An empty store ⇒ `None` ⇒ the check is inert until
        // a feed lands (decision 3); a real external adapter-gear, when registered,
        // overrides per port.
        let control_feeds = Arc::new(crate::infra::control_feed::InProcessControlFeeds::new());
        let manifest_feed: Arc<dyn IssuedInvoiceManifestV1> = ctx
            .client_hub()
            .get::<dyn IssuedInvoiceManifestV1>()
            .unwrap_or_else(|_| Arc::clone(&control_feeds) as Arc<dyn IssuedInvoiceManifestV1>);
        let bill_run_feed: Arc<dyn BillRunFinishedV1> = ctx
            .client_hub()
            .get::<dyn BillRunFinishedV1>()
            .unwrap_or_else(|_| Arc::clone(&control_feeds) as Arc<dyn BillRunFinishedV1>);
        let psp_feed: Arc<dyn PspSettlementFeedV1> = ctx
            .client_hub()
            .get::<dyn PspSettlementFeedV1>()
            .unwrap_or_else(|_| Arc::clone(&control_feeds) as Arc<dyn PspSettlementFeedV1>);
        // The pre-close completeness / bill-run gate inputs for the in-process client's
        // close path (flag-gated by `cfg.recon`); `init()`-built so the close gate reads
        // the same feeds the framework + the `…/control/*` ingest endpoints share.
        let close_control = crate::infra::period_close::CloseControlFeeds {
            manifest_feed: Arc::clone(&manifest_feed),
            bill_run_feed: Arc::clone(&bill_run_feed),
            manifest_enforcement: cfg.recon.manifest_enforcement,
            bill_run_enforcement: cfg.recon.bill_run_enforcement,
            // C3: Mode-B FX-revaluation completeness gate rides the existing Mode-B
            // enable flag (inert until Mode-B is turned on, the v1 default).
            fx_revaluation_enforcement: cfg.fx.revaluation_enabled,
        };
        // Captured before `db` / `publisher` / `metrics` move into the client below —
        // the `ReconciliationFramework` (the ticker + the `reconciliation-runs` REST
        // trigger) is built over its own clones (the jobs pattern); the
        // `reconciliation-runs` read repo gets its own db clone (mirrors the other
        // read-surface repos).
        let recon_db = db.clone();
        let recon_publisher = Arc::clone(&publisher);
        let db_for_recon_repo = db.clone();

        // The invoice-post write orchestrator backing the journal-entry REST
        // surface (post / reversal / mapping-correction). It needs repo + posting
        // access, so it wraps its own `PostingService` over clones of the same
        // db / publisher / metrics the in-process client uses (built before the
        // client move below).
        let posting_service = Arc::new(crate::infra::invoice_post::InvoicePostService::new(
            db.clone(),
            Arc::clone(&publisher),
            Arc::clone(&metrics),
            // Slice 4: the recognition derivation enforces the segment ceiling
            // from this config; the same validated `cfg.recognition` the runner
            // job reads (validated at the top of `init`).
            cfg.recognition.clone(),
            // Slice 5: the S1 FX lock resolves over the local rate store using the
            // validated `cfg.fx` (provider order + staleness windows).
            cfg.fx.clone(),
        ));

        // Typed controlled-annotation write port backing the `PATCH …/annotation`
        // surface (Group 2B). Wraps a stateless AnnotationService over a clone of
        // the same db; it opens its own SERIALIZABLE transaction (upsert +
        // secured-audit) and never touches the journal tables.
        let annotation_writer: Arc<dyn crate::infra::annotation::AnnotationWriter> = Arc::new(
            crate::infra::annotation::LedgerAnnotationWriter::new(db.clone())
                .with_metrics(Arc::clone(&metrics)),
        );

        // Audit-retrieval state (Group 2C): the scoped reader over a clone of the
        // same db, plus the cross-tenant elevation gateway. Built before the `db`
        // move into the client below.
        let audit = Arc::new(crate::api::rest::audit::ApiState {
            reader: crate::infra::audit::retrieval::AuditRetrievalReader::new(db.clone()),
            gateway: crate::infra::authz::cross_tenant::CrossTenantGateway::new()
                .with_metrics(Arc::clone(&metrics)),
            exporter: crate::infra::inquiry::AuditPackExporter::new(db.clone())
                .with_metrics(Arc::clone(&metrics)),
            erasure: crate::infra::pii::ErasureService::new().with_metrics(Arc::clone(&metrics)),
            db: db.clone(),
        });

        // Slice-3 adjustment orchestrators (Group E). Concrete handlers (not behind
        // `LedgerClientV1`): the credit-note handler wraps its own `PostingService`
        // over clones of the same db / publisher; the debit-note handler also takes
        // the SAME validated `cfg.recognition` the invoice-post / runner read (a
        // deferred debit note runs the identical schedule derivation, D4). Built
        // BEFORE `db` / `publisher` are moved into the client below.
        let credit_note_handler = Arc::new(
            crate::infra::adjustment::credit_note_service::CreditNoteHandler::new(
                db.clone(),
                Arc::clone(&publisher),
                Arc::clone(&metrics),
            )
            .with_exceptions(Arc::clone(&exception_router)),
        );
        let debit_note_handler = Arc::new(
            crate::infra::adjustment::debit_note_service::DebitNoteHandler::new(
                db.clone(),
                Arc::clone(&publisher),
                Arc::clone(&metrics),
                cfg.recognition.clone(),
            )
            .with_exceptions(Arc::clone(&exception_router)),
        );
        // The exposure-read repo for `GET …/exposure` (a plain scoped read over its
        // own db clone).
        let adjustment_repo = crate::infra::storage::repo::AdjustmentRepo::new(db.clone());

        // AM client + the Types Registry back the provisioning seller-type guard:
        // only a tenant whose TYPE owns a billing ledger may be provisioned
        // (§4.12). A missing AM client would silently skip the guard, so it
        // fails init loudly (like the enforcer).
        let am_client = ctx
            .client_hub()
            .get::<dyn account_management_sdk::AccountManagementClient>()
            .context(
                "bss-ledger: AccountManagementClient absent from ClientHub; \
                 account-management module must be registered",
            )?;
        let seller_guard = Arc::new(crate::infra::seller_guard::SellerGuard::new(
            Arc::new(crate::infra::seller_guard::AmTenantTypeReader::new(
                am_client,
            )),
            cfg.seller_tenant_types.clone(),
        ));

        // Slice 5 Phase 3: the Mode-B revaluation runner (the REST trigger's
        // handle) — built BEFORE `db`/`publisher` are moved into the local client.
        let revaluation_run = Arc::new(
            crate::infra::fx::revaluation_run::UnrealizedRevaluationRun::new(
                db.clone(),
                Arc::clone(&publisher),
                cfg.fx.clone(),
            )
            .with_metrics(Arc::clone(&metrics)),
        );

        let client: Arc<dyn LedgerClientV1> = Arc::new(LedgerLocalClient::new(
            db,
            publisher,
            enforcer,
            seller_guard,
            metrics,
            // Slice 5: the validated FX config for the in-process client's S2 settle lock.
            cfg.fx.clone(),
            // Slice 3: the validated payments config (per-allocation touched-invoice cap).
            cfg.payments.clone(),
            // Slice 7 Phase 3: the gated close consults the manifest / bill-run feeds.
            close_control,
        ));
        ctx.client_hub()
            .register::<dyn LedgerClientV1>(Arc::clone(&client));

        // Dual-control (VHP-1852): the approval lifecycle engine + its executor,
        // which replays an approved mutation through the same client / posting
        // surfaces the inline path uses (idempotent execute-then-mark).
        let approval_poster: Arc<dyn crate::infra::invoice_post::InvoicePoster> =
            posting_service.clone();
        let payer_state_repo = crate::infra::storage::repo::PayerStateRepo::new(payer_state_db);
        // The executor replays an approved refund through an UN-GATED `RefundHandler`
        // (no `approval` attached) via `post_refund_approved` — the threshold was
        // already crossed at gate time, so the replay must never re-gate. Group G
        // wires a SEPARATE gated handler (`.with_approval(...)`) onto the refund REST
        // surface for the inline preparer path.
        // The secured-audit sink for the `unknown_final` disposition (Group F /
        // Slice 6 seam). NO-OP until Slice 6 (VHP-1858) merges — it logs the
        // would-be `secured_audit_record` + bumps a metric, persisting nothing
        // durable; the real `SecuredAuditStore` binds here at merge with no
        // call-site change. Also feeds `with_metrics` so the disposition's §9
        // counter (`ledger_refund_unknown_final_total`) emits.
        let secured_audit_sink: Arc<dyn crate::infra::audit::secured_audit_sink::SecuredAuditSink> =
            Arc::new(crate::infra::audit::secured_audit_sink::NoopSecuredAuditSink::new());
        let approval_refund_handler = Arc::new(
            crate::infra::adjustment::refund_service::RefundHandler::new(
                refund_db,
                refund_publisher,
            )
            // A `RefundWithCreditNote` composite replay needs a wired
            // `CreditNoteHandler` (the orchestrator fails "composite requires a wired
            // CreditNoteHandler" otherwise). The composite posts via `apply_in_txn`,
            // which never gates, so the un-gated `credit_note_handler` is correct here.
            .with_credit_note_handler(Arc::clone(&credit_note_handler))
            .with_audit_sink(Arc::clone(&secured_audit_sink))
            .with_metrics(Arc::clone(&rt_metrics))
            .with_exceptions(Arc::clone(&exception_router)),
        );
        // The executor replays an approved manual adjustment through an UN-GATED
        // `ManualAdjustmentHandler` (no `approval` attached) via
        // `post_manual_adjustment_approved` — the threshold was already crossed at gate
        // time, so the replay must never re-gate (mirrors `approval_refund_handler`).
        // Group 6 wires a SEPARATE gated handler (`.with_approval(...)`) onto the
        // manual-adjustments REST surface for the inline preparer path.
        let approval_manual_handler = Arc::new(
            crate::infra::adjustment::manual_adjustment_service::ManualAdjustmentHandler::new(
                manual_db,
                manual_publisher,
                Arc::clone(&secured_audit_sink),
            ),
        );
        // Slice 7: the executor replays an approved `PeriodReopen` through this
        // PeriodCloseService::reopen (CLOSED→REOPENED + secured `period-reopen`
        // audit). Its own db/publisher clones + the shared no-op secured-audit sink
        // (durable at Slice 6 with no call-site change).
        let period_close_for_executor = crate::infra::period_close::PeriodCloseService::new(
            period_close_db,
            period_close_publisher,
            Arc::clone(&secured_audit_sink),
        );
        let approval_executor: Arc<dyn crate::infra::approval::service::ApprovalExecutor> =
            Arc::new(
                crate::infra::approval::executor::LedgerApprovalExecutor::new(
                    Arc::clone(&client),
                    approval_poster,
                    payer_state_repo.clone(),
                    Arc::clone(&approval_refund_handler),
                    Arc::clone(&approval_manual_handler),
                    // The note replays re-enter through `post_*_approved`, which skips
                    // the gate REGARDLESS of a wired `approval` — so the un-gated REST
                    // builder handlers serve the replay too (no separate un-gated note
                    // instance needed). The threshold was already crossed at gate time;
                    // the `*_approved` entry must never re-gate.
                    Arc::clone(&credit_note_handler),
                    Arc::clone(&debit_note_handler),
                    period_close_for_executor,
                ),
            );
        let approval_service = Arc::new(crate::infra::approval::service::ApprovalService::new(
            approval_db,
            approval_executor,
            Arc::clone(&rt_metrics),
            cfg.fx.clone(),
        ));
        // The Group-6 / Phase-3 GATED manual-adjustment REST orchestrator (its own
        // PostingService over the dedicated db/publisher clones) wired with the
        // dual-control engine (over-D2 → 409) + the no-op secured-audit sink (the
        // write-off capture, Slice-6 seam). SEPARATE instance from the executor's
        // UN-gated `approval_manual_handler` (which replays an already-approved
        // adjustment via `post_manual_adjustment_approved` and must never re-gate),
        // mirroring the refund surface's `refund_handler` vs `approval_refund_handler`.
        let manual_handler = Arc::new(
            crate::infra::adjustment::manual_adjustment_service::ManualAdjustmentHandler::new(
                manual_rest_db,
                manual_rest_publisher,
                Arc::clone(&secured_audit_sink),
            )
            .with_approval(Arc::clone(&approval_service)),
        );
        // The Slice-3 GATED credit-note + debit-note REST orchestrators (each its own
        // PostingService over the dedicated db/publisher clones) wired with the
        // dual-control engine (over-threshold → 409). SEPARATE instances from the
        // un-gated `credit_note_handler` / `debit_note_handler` above (which back the
        // refund composite + the executor replay, neither of which gates) — mirroring the
        // refund/manual gated-vs-un-gated split. The executor still replays already-
        // approved notes through the un-gated handlers via `post_*_approved` (skips the
        // gate), so these gated instances are inline-path only.
        let credit_note_handler_gated = Arc::new(
            crate::infra::adjustment::credit_note_service::CreditNoteHandler::new(
                credit_rest_db,
                credit_rest_publisher,
                Arc::clone(&rt_metrics),
            )
            .with_approval(Arc::clone(&approval_service))
            .with_exceptions(Arc::clone(&exception_router)),
        );
        let debit_note_handler_gated = Arc::new(
            crate::infra::adjustment::debit_note_service::DebitNoteHandler::new(
                debit_rest_db,
                debit_rest_publisher,
                Arc::clone(&rt_metrics),
                cfg.recognition.clone(),
            )
            .with_approval(Arc::clone(&approval_service))
            .with_exceptions(Arc::clone(&exception_router)),
        );
        let approvals = Arc::new(crate::api::rest::approvals::ApiState {
            service: Arc::clone(&approval_service),
        });
        let payers = Arc::new(crate::api::rest::payers::ApiState {
            approval: Arc::clone(&approval_service),
            payer_state: payer_state_repo,
        });
        // Slice 7 Group C: the period-closure surface reuses the in-process client
        // (the gated close + `period.closed` emit) for `close` and the dual-control
        // engine for `reopen` (always 409 DUAL_CONTROL_REQUIRED → approve → executor).
        let closure = Arc::new(crate::api::rest::closure::ApiState {
            client: Arc::clone(&client),
            approval: Arc::clone(&approval_service),
        });
        // Slice 7 Phase 2: the exception-queue dashboard surface (GET /exceptions +
        // POST /exceptions/{id}/resolution) over its own scoped repo clone.
        let exceptions = Arc::new(crate::api::rest::exceptions::ApiState {
            repo: crate::infra::storage::repo::ExceptionQueueRepo::new(db_for_exceptions),
        });

        // Build and publish the runtime (the provisioning + journal routes
        // reuse the same in-process client — no second instance; the serve loop
        // reuses the captured db/publisher clones for the background jobs).
        let provisioning = Arc::new(crate::api::rest::provisioning::ApiState {
            client: Arc::clone(&client),
        });
        let journal = Arc::new(crate::api::rest::journal_entries::ApiState {
            client: Arc::clone(&client),
            posting: posting_service,
            approval: Some(Arc::clone(&approval_service)),
            annotation: annotation_writer,
            // The R5 journal entry-HEADER read-surface repo (`GET /journal-entries`
            // list): a plain scoped read over its own db clone, called directly from
            // the handler (mirrors the dispute repo).
            journal_repo: Some(crate::infra::storage::repo::JournalRepo::new(
                db_for_journal_repo,
            )),
            posting_policy: Some(crate::infra::storage::repo::PostingPolicyRepo::new(
                db_for_posting_policy,
            )),
        });
        let posting_policy = Arc::new(crate::api::rest::posting_policy::ApiState {
            posting_policy: crate::infra::storage::repo::PostingPolicyRepo::new(
                db_for_posting_policy_rest,
            ),
        });
        let payments = Arc::new(crate::api::rest::payments::ApiState {
            client: Arc::clone(&client),
            // The R4 settlement read-surface repo (`GET /payments/{id}/settlement`):
            // a plain scoped read over its own db clone (mirrors the dispute repo).
            payment_repo: crate::infra::storage::repo::PaymentRepo::new(db_for_payment_repo),
        });
        let credit = Arc::new(crate::api::rest::credit::ApiState {
            client: Arc::clone(&client),
            approval: Some(Arc::clone(&approval_service)),
        });
        let disputes = Arc::new(crate::api::rest::disputes::ApiState {
            client: Arc::clone(&client),
            approval: Some(Arc::clone(&approval_service)),
            // The dispute read-surface repo (R3): the `GET /disputes` list +
            // `GET /disputes/{id}` by-id read source (a plain scoped read over its
            // own db clone, mirroring the refund surface's `refund_repo`).
            dispute_repo: crate::infra::storage::repo::DisputeRepo::new(db_for_dispute_repo),
        });
        // The recognition gate is taken behind the `RecognitionApprovalGate` trait
        // (the unit-test seam); coerce the concrete service into
        // the trait object at this binding site.
        let recognition_gate: Arc<dyn crate::api::rest::recognition::RecognitionApprovalGate> =
            approval_service.clone();
        let recognition = Arc::new(crate::api::rest::recognition::ApiState {
            client: Arc::clone(&client),
            approval: Some(recognition_gate),
            // The R4 recognition-run read-surface repo (`GET /recognition-runs` list
            // + `GET /recognition-runs/{run_id}` by-id): a plain scoped read over its
            // own db clone (mirrors the dispute repo).
            recognition_repo: Some(crate::infra::storage::repo::RecognitionRepo::new(
                db_for_recognition_repo,
            )),
        });
        // Slice-3 adjustment REST state: the GATED handlers built above + the
        // exposure-read repo. The credit / debit / manual note handlers are all the
        // GATED instances (dual-control over threshold → 409); the executor replays an
        // already-approved note through SEPARATE un-gated handlers via `post_*_approved`
        // (which skips the gate), mirroring the refund surface's gated/un-gated split.
        let adjustments = Arc::new(crate::api::rest::adjustments::ApiState {
            credit: credit_note_handler_gated,
            debit: debit_note_handler_gated,
            manual: manual_handler,
            exposure_repo: adjustment_repo,
        });
        // Slice-3 Phase-2 Group-G refund REST state: a GATED `RefundHandler` (its own
        // PostingService over the same db/publisher) wired with the dual-control
        // engine (over-D2 → 409), the composite credit-note handler (the atomic
        // `refund-with-credit-note`), the no-op secured-audit sink (the `unknown_final`
        // disposition, Slice-6 seam), and the metrics meter. SEPARATE instance from
        // the executor's UN-gated `approval_refund_handler` (which replays an
        // already-approved refund and must never re-gate). The exposure repo is reused
        // for the `GET /refunds/{id}` read (a plain scoped read).
        let refund_handler = Arc::new(
            crate::infra::adjustment::refund_service::RefundHandler::new(
                db_for_refunds,
                Arc::clone(&publisher_for_refunds),
            )
            .with_approval(Arc::clone(&approval_service))
            .with_credit_note_handler(Arc::clone(&credit_note_handler))
            .with_audit_sink(Arc::clone(&secured_audit_sink))
            .with_metrics(Arc::clone(&rt_metrics))
            .with_exceptions(Arc::clone(&exception_router)),
        );
        let refunds = Arc::new(crate::api::rest::refunds::ApiState {
            refunds: refund_handler,
            refund_repo: crate::infra::storage::repo::AdjustmentRepo::new(db_for_refund_repo),
        });
        // Slice-5 FX REST state: its own clone of the FX repo (the runtime's
        // `fx_repo` below is consumed by the RateSyncJob; `FxRepo` is `Clone`).
        let fx = Arc::new(crate::api::rest::fx::ApiState {
            fx_repo: fx_repo.clone(),
            revaluation_run: Arc::clone(&revaluation_run),
            fx_revaluation_mode: crate::infra::storage::repo::FxRevaluationModeRepo::new(
                db_for_fx_revaluation_mode,
            ),
            fleet_revaluation_enabled: cfg.fx.revaluation_enabled,
        });
        // VHP-1986: the per-tenant FX revaluation-mode config REST surface (its own
        // repo clone; mirrors the posting-policy config surface).
        let fx_revaluation_mode = Arc::new(crate::api::rest::fx_revaluation_mode::ApiState {
            fx_revaluation_mode: crate::infra::storage::repo::FxRevaluationModeRepo::new(
                db_for_fx_revaluation_mode_rest,
            ),
        });
        // Slice 7 Phase 3: the reconciliation framework (AR↔derived / Payments↔PSP /
        // invoice-completeness) over its own db/publisher clones + the shared metrics +
        // exception router + the resolved control feeds + the recon config. Driven by the
        // `ReconciliationJob` ticker (serve loop, via `reconciliation.framework`) + the
        // `reconciliation-runs` REST trigger.
        let reconciliation_framework =
            Arc::new(crate::infra::reconciliation::ReconciliationFramework::new(
                recon_db,
                recon_publisher,
                Arc::clone(&rt_metrics),
                Arc::clone(&exception_router),
                Arc::clone(&manifest_feed),
                Arc::clone(&psp_feed),
                cfg.recon.clone(),
            ));
        let reconciliation = Arc::new(crate::api::rest::reconciliation::ApiState {
            framework: reconciliation_framework,
            run_repo: crate::infra::storage::repo::ReconciliationRunRepo::new(db_for_recon_repo),
        });
        // The control-feed ingest surface pushes into the SAME in-process store the
        // framework + close gate read back.
        let control = Arc::new(crate::api::rest::control::ApiState {
            feeds: Arc::clone(&control_feeds),
        });
        self.runtime.store(Some(Arc::new(LedgerRuntime {
            provisioning,
            journal,
            payments,
            credit,
            disputes,
            recognition,
            adjustments,
            approvals,
            posting_policy,
            fx_revaluation_mode,
            approval: Arc::clone(&approval_service),
            refunds,
            payers,
            closure,
            exceptions,
            exception_router,
            reconciliation,
            control,
            audit,
            fx,
            enforcer: rt_enforcer,
            db: jobs_db,
            publisher: jobs_publisher,
            metrics: rt_metrics,
            tie_out_tick: cfg.jobs.tie_out_interval(),
            tie_out_full_every_n: cfg.jobs.tieout_full_every_n,
            period_open_tick: cfg.jobs.period_open_interval(),
            queue_applier_tick: cfg.jobs.queue_applier_interval(),
            aged_alarm_tick: cfg.jobs.aged_alarm_interval(),
            recognition_run_tick: cfg.recognition.recognition_run_interval(),
            verify_tick: cfg.jobs.verify_interval(),
            rate_provider_hub,
            rate_provider_vendor,
            fx_repo,
            fx_rate_sync_tick: cfg.fx.rate_sync_interval(),
            fx_config: cfg.fx.clone(),
            payments_config: cfg.payments.clone(),
            revaluation_run_tick: cfg.fx.revaluation_run_interval(),
            recon_tick: cfg.recon.recon_tick_interval(),
        })));

        info!("bss-ledger: published LedgerClientV1 in ClientHub");
        Ok(())
    }
}

/// Build the [`LedgerEventPublisher`] at `init()`.
///
/// TODO(broker): the event broker (`event-broker-sdk`) is not yet available in
/// gears-rust. Until it lands this returns a **broker-free** publisher — it
/// mirrors the invariant-alarm counter into metrics and logs would-be events,
/// but publishes nothing. When the broker arrives, restore here: obtain
/// `hub.get::<dyn event_broker_sdk::EventBroker>()`, register the event-type
/// schemas (`crate::infra::events::schemas::register_event_schemas`), build one
/// `AsyncProducer` per event type via `broker.producer_builder()...build_async()`,
/// and return `LedgerEventPublisher::new(<producers>, db, metrics)`.
async fn build_event_publisher(
    _ctx: &GearCtx,
    _db: &DBProvider<DbError>,
    events_enabled: bool,
    metrics: Arc<dyn LedgerMetricsPort>,
) -> LedgerEventPublisher {
    if !events_enabled {
        warn!("bss-ledger: events disabled (events_enabled=false); no-op publisher");
        return LedgerEventPublisher::noop();
    }
    // Broker parked (no event-broker-sdk in gears-rust yet): a metrics-only
    // publisher keeps the invariant-alarm counter live; every event publish is a
    // logged no-op (see `LedgerEventPublisher`). No broker dependency.
    LedgerEventPublisher::with_metrics(metrics)
}

/// `DatabaseCapability` impl. Returns the migration list so `toolkit`
/// runs migrations at platform startup before any ledger code reads the DB.
impl DatabaseCapability for BssLedgerGear {
    fn migrations(&self) -> Vec<Box<dyn MigrationTrait>> {
        Migrator::migrations()
    }
}

/// `RestApiCapability` impl. Mounts the provisioning route when `init()` has
/// populated the runtime; on a default-disabled boot it falls back to an
/// empty `/v1` mount.
impl RestApiCapability for BssLedgerGear {
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: Router,
        openapi: &dyn OpenApiRegistry,
    ) -> Result<Router> {
        if let Some(rt) = self.runtime.load_full() {
            Ok(router
                .merge(crate::api::rest::provisioning::router(
                    Arc::clone(&rt.provisioning),
                    openapi,
                ))
                .merge(crate::api::rest::journal_entries::router(
                    Arc::clone(&rt.journal),
                    openapi,
                ))
                .merge(crate::api::rest::payments::router(
                    Arc::clone(&rt.payments),
                    openapi,
                ))
                .merge(crate::api::rest::credit::router(
                    Arc::clone(&rt.credit),
                    openapi,
                ))
                .merge(crate::api::rest::disputes::router(
                    Arc::clone(&rt.disputes),
                    openapi,
                ))
                .merge(crate::api::rest::recognition::router(
                    Arc::clone(&rt.recognition),
                    openapi,
                ))
                .merge(crate::api::rest::adjustments::router(
                    Arc::clone(&rt.adjustments),
                    openapi,
                ))
                .merge(crate::api::rest::refunds::router(
                    Arc::clone(&rt.refunds),
                    openapi,
                ))
                .merge(crate::api::rest::approvals::router(
                    Arc::clone(&rt.approvals),
                    openapi,
                ))
                .merge(crate::api::rest::posting_policy::router(
                    Arc::clone(&rt.posting_policy),
                    openapi,
                ))
                .merge(crate::api::rest::fx_revaluation_mode::router(
                    Arc::clone(&rt.fx_revaluation_mode),
                    openapi,
                ))
                .merge(crate::api::rest::payers::router(
                    Arc::clone(&rt.payers),
                    openapi,
                ))
                .merge(crate::api::rest::closure::router(
                    Arc::clone(&rt.closure),
                    openapi,
                ))
                .merge(crate::api::rest::exceptions::router(
                    Arc::clone(&rt.exceptions),
                    openapi,
                ))
                .merge(crate::api::rest::audit::router(
                    Arc::clone(&rt.audit),
                    openapi,
                ))
                .merge(crate::api::rest::fx::router(Arc::clone(&rt.fx), openapi))
                .merge(crate::api::rest::reconciliation::router(
                    Arc::clone(&rt.reconciliation),
                    openapi,
                ))
                .merge(crate::api::rest::control::router(
                    Arc::clone(&rt.control),
                    openapi,
                ))
                // Per-request PEP for the handlers (RMS layers the value, not
                // the `Arc`; `PolicyEnforcer: Clone`).
                .layer(axum::Extension((*rt.enforcer).clone()))
                .layer(axum::middleware::from_fn(
                    toolkit::api::canonical_error_middleware,
                )))
        } else {
            Ok(router.nest("/bss-ledger/v1", Router::new()))
        }
    }
}

#[cfg(test)]
#[path = "module_tests.rs"]
mod tests;
