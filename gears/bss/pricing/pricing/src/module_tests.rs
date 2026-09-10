//! The background plane: its supervision, its leases, and the port its jobs are
//! handed.
//!
//! # The three claims here, and why they are one file
//!
//! Z10-12 recorded that nothing tested the scheduling plane at all — every job body
//! was covered and the code deciding *when and under what lease a body runs* was
//! not. Three defects lived in that gap: a guard dropped rather than released
//! (halving a cadence), a panic invisible until shutdown, and the `with_metrics`
//! attachment. The first two are asserted below without a database; the other two
//! claims need one, which is why they were left to their own task and land here now.
//!
//! What still has no case is `serve`'s own `select!` — the arms are covered through
//! [`BssPricingGear::exited_first`], but reaching `serve` needs a whole
//! [`PricingRuntime`], and a runtime needs a PEP, a registry and eight API states.
//! The wiring decisions each ticker used to make inside `tokio::spawn` were moved
//! out to named functions for exactly that reason.
//!
//! # The background plane's supervision — what happens when a ticker dies
//!
//! `serve` spawns three tickers and, until 2026-08-11, awaited their handles
//! **only after** cancellation. A panic on tick 1 therefore left the gear serving
//! traffic with `serve` still `Ok(())`, and the only trace was a `warn` emitted
//! whenever the process finally stopped.
//!
//! That is not cosmetic here. `serve`'s own doc says the warm re-drive is what
//! resolves a pending `CatalogVersion` handle — without it `pricing_read_model`
//! stays empty and no version ever becomes pin-eligible — and the two Criticals
//! `readmodel_warm` raises cannot fire, because the task that raises them is the
//! dead one.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use crate::domain::ports::unconfigured_registry;
use std::sync::Arc;
use std::time::Duration;
use toolkit_canonical_errors::CanonicalError;

use async_trait::async_trait;
use bss_pricing_sdk::CatalogVersion;
use bss_pricing_sdk::catalog_version_registry::{CatalogVersionRegistryV1, PendingVersionRef};

use sea_orm_migration::MigratorTrait as _;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{BssPricingGear, GATED_MARKETS_LEASE_KEY, WARM_LEASE_KEY, WINDOW_ACTIVATION_LEASE_KEY};
use crate::config::JobsConfig;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::ports::metrics::PricingMetricsPort;
use crate::domain::read_model::SubjectRef;
use crate::infra::metrics::test_harness::MetricsHarness;
use crate::infra::storage::migrations::Migrator;
use crate::infra::storage::repo::{PendingVersionRow, catalog_version_ref_repo};
use time::OffsetDateTime;

/// A handle that has already panicked, for the arm under test.
async fn a_panicking_ticker() -> tokio::task::JoinHandle<()> {
    let handle = tokio::spawn(async {
        panic!("a sweep panicked");
    });
    // Let it land, so the arm sees a resolved `Err` rather than a pending future.
    tokio::task::yield_now().await;
    handle
}

/// A handle that ends cleanly, standing in for a surviving sibling.
fn a_surviving_ticker() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {})
}

/// **A panicking ticker fails `serve` rather than being logged at shutdown.**
///
/// The assertion is on the returned `Err` and on the ticker it names: a
/// supervision arm that reported *something* went wrong without saying which
/// plane died would leave an operator reading three tickers' logs to find out.
#[tokio::test]
async fn a_panicking_ticker_is_reported_through_serves_return() {
    let dead = a_panicking_ticker().await;

    let outcome = BssPricingGear::exited_first(
        "readmodel-warm",
        dead.await,
        vec![
            ("window-activation", a_surviving_ticker()),
            ("gated-markets", a_surviving_ticker()),
        ],
    )
    .await;

    let err = outcome.expect_err("a panicked ticker must not read as a clean stop");
    let rendered = err.to_string();
    assert!(
        rendered.contains("readmodel-warm"),
        "the failure must name the ticker that died: {rendered}"
    );
}

/// **A ticker that returns without panicking is also a failure**, and that is a
/// decision rather than an oversight.
///
/// The loop shape runs until the shared token is cancelled and catches every tick
/// failure itself, so a clean early return is a state its own code does not
/// produce. Treating it as a normal stop would put the background plane back
/// exactly where this arm found it: silently absent, with `serve` reporting
/// healthy.
#[tokio::test]
async fn a_ticker_that_stops_early_without_panicking_is_still_a_failure() {
    let quiet = a_surviving_ticker();

    let outcome = BssPricingGear::exited_first(
        "gated-markets",
        quiet.await,
        vec![
            ("readmodel-warm", a_surviving_ticker()),
            ("window-activation", a_surviving_ticker()),
        ],
    )
    .await;

    let err = outcome.expect_err("a ticker stopping before cancellation is not a clean stop");
    assert!(err.to_string().contains("gated-markets"), "{err}");
}

/// A sibling's panic is reported even when the ticker that woke the arm was fine.
///
/// The survivors are drained rather than abandoned — each holds a coordination
/// lease — and draining them is worth nothing if what the drain finds is
/// discarded.
///
/// **Asserted on the rendered error, not on `is_err()`.** Until 2026-08-20 it was
/// the latter, and `is_err()` is not an operand of anything here: *every* arm of
/// [`BssPricingGear::exited_first`] returns `Err`, and the ticker that wakes this
/// one is handed `quiet.await` — a clean early return, which the case above
/// proves is itself reported as a failure. So the sibling's `JoinError` was never
/// read by any assertion, and deleting both `.or_else(|| survivors.N.err() …)`
/// arms reddened nothing in the crate: the case covering the drain-and-report
/// behaviour was green against its own removal.
///
/// The dead sibling's **own name** is what pins which arm answered: it is a name the
/// woken ticker's clean-return arm can never produce, and the *absence* of that
/// arm's wording is the other half.
#[tokio::test]
async fn a_siblings_panic_is_not_discarded_while_draining() {
    let quiet = a_surviving_ticker();
    let dead = a_panicking_ticker().await;

    let outcome = BssPricingGear::exited_first(
        "window-activation",
        quiet.await,
        vec![
            ("gated-markets", dead),
            ("repricing-compensation", a_surviving_ticker()),
        ],
    )
    .await;

    let err = outcome.expect_err("a panic found while draining the survivors must not be dropped");
    let rendered = err.to_string();
    assert!(
        rendered.contains("gated-markets"),
        "the drain must name the sibling that panicked, not report it anonymously: {rendered}"
    );
    assert!(
        !rendered.contains("stopped before cancellation"),
        "and not the woken ticker's clean return, which is a different arm and \
         would report an error either way: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// What a warm pass tells an operator (Z13-7).
// ---------------------------------------------------------------------------

/// **A pass that did nothing is silent, and a pass that did something is not.**
///
/// `SweepReport`'s eleven counters were dropped whole by the only production caller
/// of `run`, so the sweep `serve` calls the one *"without which `pricing_read_model`
/// stays empty"* produced no per-pass signal while its less load-bearing sibling did.
/// The rule that decides is asserted here rather than the emission, because this
/// crate has no tracing capture — `log_activation` has carried the same rule inline
/// since the sweep landed and has never had a case.
///
/// Every arm is driven separately. A table over one field at a time is what catches
/// the arm that was left out of the disjunction: a single "everything at once" report
/// passes with any one of the seven checks present.
#[test]
fn a_warm_pass_is_worth_logging_exactly_when_it_moved_or_failed_at_something() {
    use crate::infra::jobs::readmodel_warm::SweepReport;

    // The steady state twice over: nothing pending, and tenants swept whose refs are
    // simply still inside the batching budget. At a 5s cadence either of these
    // logging at `info` would bury every pass that did something.
    assert!(
        !BssPricingGear::sweep_is_noteworthy(&SweepReport::default()),
        "a pass with nothing pending is the steady state"
    );
    assert!(
        !BssPricingGear::sweep_is_noteworthy(&SweepReport {
            tenants_seen: 3,
            pending_seen: 9,
            ..SweepReport::default()
        }),
        "seeing tenants and refs is not doing anything with them: this is every tick \
         inside D-47's batching budget"
    );
    // A deployment state, not an event. `readmodel_warm` puts it at `debug` because
    // the e2e that boots this gear with no registry has to stay readable, and every
    // pass is inert forever in that deployment.
    assert!(
        !BssPricingGear::sweep_is_noteworthy(&SweepReport {
            inert: true,
            tenants_seen: 1,
            ..SweepReport::default()
        }),
        "an unconfigured registry must not log at `info` twelve times a minute"
    );

    for (report, what) in [
        (
            SweepReport {
                versions_projected: 1,
                ..SweepReport::default()
            },
            "a version was projected",
        ),
        (
            SweepReport {
                subjects_failed: 1,
                ..SweepReport::default()
            },
            "a subject's transaction refused",
        ),
        (
            SweepReport {
                frontiers_advanced: 1,
                ..SweepReport::default()
            },
            "a frontier advanced",
        ),
        (
            SweepReport {
                degraded_emitted: 1,
                ..SweepReport::default()
            },
            "a PlanPublishDegraded was enqueued",
        ),
        (
            SweepReport {
                commit_overdue: 1,
                ..SweepReport::default()
            },
            "a Critical was raised",
        ),
        (
            SweepReport {
                pin_eligibility_overdue: 1,
                ..SweepReport::default()
            },
            "the other Critical was raised",
        ),
        (
            // The one that is not a counter, and the one Z10-5 added: a Critical
            // that could not be *evaluated* leaves every other counter identical to
            // a healthy pass's, so it is the arm most easily left out.
            SweepReport {
                frontier_scan_failed: true,
                ..SweepReport::default()
            },
            "the cross-tenant frontier read failed",
        ),
        (
            // Z4-2's, and it is the arm that was left out when Z10-5's was
            // added: `degraded_emitted` does not move when the enqueue fails, so
            // a pass that lost a `PlanPublishDegraded` was byte-identical here to
            // one that had none to send.
            SweepReport {
                degraded_emit_failed: true,
                ..SweepReport::default()
            },
            "a PlanPublishDegraded could not be enqueued",
        ),
        (
            // Z4-8's. Reached only where the tenant's pending refs are all inside
            // the SLO — so on this path there is no ref alarm to fall back on,
            // which is the argument `frontier_is_blocked` used to make and could
            // not support.
            SweepReport {
                frontier_block_probe_failed: true,
                ..SweepReport::default()
            },
            "the committed-version probe behind pin-eligibility could not read",
        ),
    ] {
        assert!(
            BssPricingGear::sweep_is_noteworthy(&report),
            "a pass where {what} must reach an operator"
        );
    }
}

// ---------------------------------------------------------------------------
// The lease, over two passes of one `LeaseManager`.
// ---------------------------------------------------------------------------

/// A migrated in-memory database. The `coord` lease migration is spliced into this
/// gear's own `Migrator` (`infra::storage::migrations` says why), so the lease table
/// arrives with the Foundation chain and no second runner is needed.
async fn migrated_provider() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    DBProvider::<DbError>::new(db)
}

/// A gated-markets job over an empty catalog, on `metrics`.
fn gated_job(
    provider: &DBProvider<DbError>,
    metrics: &Arc<dyn PricingMetricsPort>,
) -> crate::infra::jobs::gated_markets::GatedMarketsJob {
    crate::infra::jobs::gated_markets::GatedMarketsJob::new(
        provider.clone(),
        Arc::clone(metrics),
        JobsConfig::default(),
    )
}

/// **Every leased pass frees its slot, so the next tick acquires it.**
///
/// The lease half of Z10-12, and it is armed against Z10-2 rather than against a
/// hypothesis: `gated_markets_pass` called `drop(guard)`. `LeaseGuard` has no `Drop`
/// impl — releasing is async DB I/O — so the row stood until its TTL, and every TTL
/// on this plane **is** the tick. The slot was claimed at `T+δ`, the next tick at
/// `T+60` found `locked_until` still ahead of it, took the `LeaseHeld` arm, and
/// logged "a peer holds its lease" at debug while naming a peer where the holder was
/// this same task's previous pass. The gauge refreshed every *other* tick.
///
/// **Driven through the three `*_pass` functions and not through `take_lease` /
/// `release_lease`**, which is where the first draft of this case had it: those two
/// are one shared acquire and one shared release, and the decision Z10-2 got wrong
/// is the *call* — in one of three look-alike pass bodies. A probe over the helpers
/// would have been green against the defect it names.
///
/// Each pass runs against a real database and an empty catalog, so it takes its
/// lease, does its (empty) work and releases. The TTL is a minute, so nothing here
/// can pass by the slot merely expiring.
#[tokio::test]
async fn every_leased_pass_frees_its_slot_for_the_next_tick() {
    let provider = migrated_provider().await;
    let lease = coord::LeaseManager::new(provider.db());
    let ttl = Duration::from_mins(1);
    let metrics: Arc<dyn PricingMetricsPort> =
        Arc::new(crate::domain::ports::metrics::NoopPricingMetrics);

    // The control comes first, so a broken `take_lease` cannot make the three
    // assertions below vacuous. A guard that is merely dropped is exactly the
    // pre-fix state, and the next acquire on its key must be refused.
    let dropped = BssPricingGear::take_lease(&lease, WARM_LEASE_KEY, ttl).await;
    assert!(dropped.is_some(), "a free slot is acquired");
    drop(dropped);
    assert!(
        BssPricingGear::take_lease(&lease, WARM_LEASE_KEY, ttl)
            .await
            .is_none(),
        "a dropped guard holds its row until the TTL, so if this acquires, the lease is not a \
         lease and every assertion below measures nothing"
    );

    // A fresh database, because the key above is now held for a minute.
    let provider = migrated_provider().await;
    let lease = coord::LeaseManager::new(provider.db());

    let warm = BssPricingGear::warm_job(
        provider.clone(),
        Arc::new(NotYetRegistry),
        JobsConfig::default(),
        Arc::clone(&metrics),
    );
    BssPricingGear::warm_pass(&lease, &warm, ttl).await;
    assert!(
        BssPricingGear::take_lease(&lease, WARM_LEASE_KEY, ttl)
            .await
            .is_some(),
        "the warm pass must release its slot; a held one is the next tick skipping itself"
    );

    let activation = BssPricingGear::activation_job(
        provider.clone(),
        JobsConfig::default(),
        Arc::clone(&metrics),
    );
    BssPricingGear::activation_pass(&lease, &activation, ttl).await;
    assert!(
        BssPricingGear::take_lease(&lease, WINDOW_ACTIVATION_LEASE_KEY, ttl)
            .await
            .is_some(),
        "the activation pass must release its slot"
    );

    let gated = gated_job(&provider, &metrics);
    BssPricingGear::gated_markets_pass(&lease, &gated, ttl).await;
    assert!(
        BssPricingGear::take_lease(&lease, GATED_MARKETS_LEASE_KEY, ttl)
            .await
            .is_some(),
        "the gated-markets pass must release its slot; this is the one that did not (Z10-2)"
    );
}

/// **Two ticks of the gated refresher publish twice**, which is Z10-2 by its own
/// number rather than by the state of a lease row.
///
/// The defect's visible cost was a gauge refreshed every *other* tick — 120s against
/// the 60s D-250 ratifies — and the state that produced it is a slot still held by
/// this task's own previous pass. So this drives two passes over **one**
/// `LeaseManager`, which is what the ticker holds (a per-pass manager mints a fresh
/// `locked_by` and could not observe this at all), and counts the publications.
///
/// The count is the instrument because the value cannot be: the gauge is written
/// with the same number on every pass over one catalog, so "refreshed twice" and
/// "refreshed once" are indistinguishable from what was written.
/// `gated_markets_tests` asserts by call count for the same reason.
#[tokio::test]
async fn two_gated_market_ticks_publish_twice_rather_than_every_other_tick() {
    #[derive(Default)]
    struct CountingGauge(std::sync::atomic::AtomicUsize);
    impl PricingMetricsPort for CountingGauge {
        fn preview_failclosed(&self, _reason: crate::domain::ports::metrics::PreviewFailClosed) {}
        fn currency_binding_block(
            &self,
            _case: crate::domain::ports::metrics::CurrencyBindingCase,
        ) {
        }
        fn tax_not_sellable_ga(&self, _count: i64) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        fn alarm(&self, _alarm: crate::domain::ports::metrics::PricingAlarm) {}
    }

    let provider = migrated_provider().await;
    let lease = coord::LeaseManager::new(provider.db());
    // The TTL the ticker passes **is** its tick (`gated_markets_interval()`), and
    // the whole defect lived in that equality, so the case uses the same shape.
    let ttl = Duration::from_mins(1);
    let counter = Arc::new(CountingGauge::default());
    let metrics: Arc<dyn PricingMetricsPort> = Arc::clone(&counter) as Arc<dyn PricingMetricsPort>;
    let job = gated_job(&provider, &metrics);

    BssPricingGear::gated_markets_pass(&lease, &job, ttl).await;
    BssPricingGear::gated_markets_pass(&lease, &job, ttl).await;

    assert_eq!(
        counter.0.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "two ticks inside one TTL must publish twice; one is the gauge refreshing at half \
         the ratified cadence with a debug line blaming a peer"
    );
}

/// The three keys are three slots, so one sweep skipping never skips another.
///
/// `module.rs` states this as the reason there is a second and a third key rather
/// than a share of the first — *"one key would make them a queue, and a window
/// boundary would then wait on a registry that is not answering"* — and the
/// statement was unmeasured. Held all three at once, which is the configuration the
/// claim is about.
#[tokio::test]
async fn the_three_sweeps_hold_three_independent_slots() {
    let provider = migrated_provider().await;
    let lease = coord::LeaseManager::new(provider.db());
    let ttl = Duration::from_mins(1);

    let warm = BssPricingGear::take_lease(&lease, WARM_LEASE_KEY, ttl).await;
    let window = BssPricingGear::take_lease(&lease, WINDOW_ACTIVATION_LEASE_KEY, ttl).await;
    let gated = BssPricingGear::take_lease(&lease, GATED_MARKETS_LEASE_KEY, ttl).await;

    assert!(
        warm.is_some() && window.is_some() && gated.is_some(),
        "each sweep holds its own key: warm={}, window={}, gated={}",
        warm.is_some(),
        window.is_some(),
        gated.is_some()
    );
    // And the keys are distinct strings, which is what makes the three slots three
    // rows. Asserted because a copy-paste of the constant would leave the acquire
    // above passing for the first two and failing for nobody until a deployment
    // found one plane queued behind another.
    assert_ne!(WARM_LEASE_KEY, WINDOW_ACTIVATION_LEASE_KEY);
    assert_ne!(WARM_LEASE_KEY, GATED_MARKETS_LEASE_KEY);
    assert_ne!(WINDOW_ACTIVATION_LEASE_KEY, GATED_MARKETS_LEASE_KEY);
}

// ---------------------------------------------------------------------------
// The `with_metrics` attachment.
// ---------------------------------------------------------------------------

/// A registry that answers *not committed yet* for every handle.
///
/// **Not** `UnconfiguredCatalogVersionRegistryV1`: that answer ends the pass at
/// `inert` before any signal is observed, so a probe using it would assert about a
/// pass that did nothing. `Ok(None)` is the ordinary batching wait, which is the
/// state a ref ages in.
struct NotYetRegistry;

#[async_trait]
impl CatalogVersionRegistryV1 for NotYetRegistry {
    async fn request_version(
        &self,
        _ctx: &SecurityContext,
        _request_id: &str,
    ) -> Result<PendingVersionRef, CanonicalError> {
        // The sweep never requests — `module.rs` records that the engine is the one
        // requester and the sweep only reads — so this arm is unreachable from this
        // case and says so rather than inventing a ref.
        Err(unconfigured_registry())
    }

    async fn committed_version(
        &self,
        _ctx: &SecurityContext,
        _pending_ref: &str,
    ) -> Result<Option<CatalogVersion>, CanonicalError> {
        Ok(None)
    }
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2099, 3, 4, hour, 0, 0)
}

/// **The job the lifecycle builds reports on the port the lifecycle hands it.**
///
/// The `with_metrics` half of Z10-12. `ReadModelWarmJob::new` installs
/// `NoopPricingMetrics`, `with_metrics` is a separate call, and every job-level suite
/// builds its own job — so a lifecycle that dropped the attachment would leave this
/// gear's only two **Critical** alarms reporting to nothing, with every existing test
/// green. Verified against `BssPricingGear::warm_job`, which is where that decision
/// now lives.
///
/// # The seam is the real adapter, and that is deliberate
///
/// This week a harness held a **no-op metrics seam** under a comment claiming the
/// real adapter, so a production counter was unobservable in every test by
/// construction. So this case takes no spy: `MetricsHarness` binds
/// `PricingMetricsMeter` — the same type `init()` builds — to a real
/// `SdkMeterProvider` with an in-memory exporter, and the assertion reads the
/// **exported** stream by instrument name and label. The `alarm` label value is
/// transcribed rather than taken from `PricingAlarm::as_str`, for
/// `readmodel_warm_tests`' reason: it is the string an operator's runbook greps for.
///
/// The seed is one pending ref an hour old against the 300s `commit_overdue`
/// threshold, never observed committed — §3.6's predicate exactly, and the pass is
/// the only thing that can raise it.
#[tokio::test]
async fn the_warm_job_the_lifecycle_builds_reports_on_the_metrics_port_it_is_handed() {
    let provider = migrated_provider().await;
    let conn = provider.conn().expect("conn");
    let tenant_id = Uuid::from_u128(0x5e_ed);
    catalog_version_ref_repo::record_pending(
        &conn,
        &AccessScope::allow_all(),
        PendingVersionRow::for_subject(
            tenant_id,
            "pend-overdue".to_owned(),
            &SubjectRef::Plan(Uuid::from_u128(0x9_1a4)),
            Some(0),
            None,
            at(9),
        ),
    )
    .await
    .expect("record a pending ref");

    let harness = MetricsHarness::new();
    let metrics: Arc<dyn PricingMetricsPort> = Arc::new(harness.metrics());
    let job = BssPricingGear::warm_job(
        provider.clone(),
        Arc::new(NotYetRegistry),
        JobsConfig::default(),
        Arc::clone(&metrics),
    );

    // An hour after the request, against a 300s threshold.
    let report = job.run(at(10)).await.expect("the pass runs");
    harness.force_flush();

    // The pass did the thing the counter is supposed to be counting. Without this,
    // a zero below would be indistinguishable from a pass that never evaluated the
    // condition — and `inert` is the way that happens by accident.
    assert!(
        !report.inert,
        "a registry answering `Ok(None)` is not inert"
    );
    assert_eq!(
        report.commit_overdue, 1,
        "one ref, an hour old, never observed committed: sec 3.6's predicate holds"
    );
    assert_eq!(
        harness.counter_value(
            "pricing_alarm_total",
            &[
                ("alarm", "pricing.catalogversion.commit_overdue"),
                ("severity", "critical")
            ]
        ),
        1,
        "the alarm must reach the port the lifecycle attached; a dropped \
         `with_metrics` reads 0 here while the report above still reads 1"
    );
}

// ---------------------------------------------------------------------------
// The SKU suggestion source, and the provenance the surface reads first.
// ---------------------------------------------------------------------------

/// A registry that answers, and answers **nothing**.
///
/// The case the provenance exists for: an empty list from a registry that was
/// asked is the opposite fact from an empty list because nobody could be, and
/// `CatalogSkusView::source` is the only thing that tells them apart.
struct EmptyRegistryCatalog;

#[async_trait]
impl crate::domain::ports::ProductCatalogClientV1 for EmptyRegistryCatalog {
    async fn list_tax_categories(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<
        Vec<crate::domain::ports::CatalogTaxCategory>,
        toolkit_canonical_errors::CanonicalError,
    > {
        Ok(Vec::new())
    }

    async fn list_skus(
        &self,
        _ctx: &SecurityContext,
    ) -> Result<Vec<crate::domain::ports::CatalogSku>, toolkit_canonical_errors::CanonicalError>
    {
        Ok(Vec::new())
    }
}

/// A config provider that knows about no gear at all, so `ctx.config()` yields
/// the all-defaults config — i.e. `product_catalog.mode = unconfigured`, which is
/// the mode a re-derived `source` would have reported.
struct NoConfig;

impl toolkit::config::ConfigProvider for NoConfig {
    fn get_gear_config(&self, _gear: &str) -> Option<&serde_json::Value> {
        None
    }
}

fn ctx_with(hub: toolkit::client_hub::ClientHub) -> toolkit::GearCtx {
    toolkit::GearCtx::new(
        "bss-pricing",
        Uuid::new_v4(),
        std::sync::Arc::new(NoConfig),
        std::sync::Arc::new(hub),
        tokio_util::sync::CancellationToken::new(),
    )
}

/// **A registered registry answering an empty list reports `registry`.**
///
/// Deriving `source` from `config.product_catalog.mode` at the call site reports
/// this deployment — a registered client, no mode in the file — as `unconfigured`:
/// "nobody was asked", about an answer the registry gave, because
/// `resolve_product_catalog` tries the `ClientHub` **first**. It also leaves the
/// documented third value unreachable, which is why this case asserts the token
/// itself rather than merely that it is not `unconfigured`.
#[test]
fn a_registered_catalog_reports_registry_even_when_it_sells_nothing() {
    let hub = toolkit::client_hub::ClientHub::new();
    hub.register::<dyn crate::domain::ports::ProductCatalogClientV1>(std::sync::Arc::new(
        EmptyRegistryCatalog,
    ));
    let ctx = ctx_with(hub);

    let (_catalog, source) =
        super::resolve_product_catalog(&ctx, &crate::config::BssPricingConfig::default());

    assert_eq!(source, "registry");
}

/// The two fallback tokens, and that neither of them is `registry`.
///
/// The positive control for the case above: without it a `resolve_product_catalog`
/// that answered `registry` unconditionally would satisfy it, and `registry` on a
/// deployment nobody wired is the same lie in the other direction.
#[test]
fn an_unwired_deployment_reports_its_mode_and_never_registry() {
    for (mode, expected) in [
        (
            crate::config::ProductCatalogSource::Unconfigured,
            "unconfigured",
        ),
        (
            crate::config::ProductCatalogSource::LocalDevStaticSkus,
            "local_dev_static",
        ),
    ] {
        let config = crate::config::BssPricingConfig {
            product_catalog: crate::config::ProductCatalogConfig { mode },
            ..crate::config::BssPricingConfig::default()
        };

        let (_catalog, source) = super::resolve_product_catalog(
            &ctx_with(toolkit::client_hub::ClientHub::new()),
            &config,
        );

        assert_eq!(source, expected);
        assert_ne!(source, "registry");
    }
}
