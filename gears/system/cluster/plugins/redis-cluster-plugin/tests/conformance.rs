//! Layer 2 — the conformance suite (docs/TESTING.md §3), wired against real
//! Redis containers.
//!
//! Passing these is the primary signal that this plugin implements the
//! `ClusterCacheBackend` and `DistributedLockBackend` contracts: the scenario
//! bodies are the shared, backend-agnostic ones every cluster backend runs, so a
//! divergence here is a divergence from the contract rather than from this
//! plugin's own expectations.
//!
//! # Four suites, and four is the complete set
//!
//! `cluster-conformance` exports six `run_*_conformance` entry points and only
//! four of them take a backend factory. `run_restart_conformance` and
//! `run_watch_lifecycle_conformance` exercise SDK-level combinator logic against
//! channels — there is no backend to hand them — so the four below are everything
//! a plugin can wire, not a subset someone stopped at.
//!
//! # `TimeControl::Real`, never `Virtual`
//!
//! Every suite passes [`TimeControl::Real`]. A paused or auto-advancing clock is
//! not merely unnecessary against a real backend, it is actively wrong: `fred`
//! runs its own per-command timeouts and its reconnect backoff on this same
//! runtime, and `tokio`'s paused clock auto-advances to the next pending timer
//! deadline whenever the runtime has nothing to poll — which, while a real socket
//! read is parked, is constantly. Those timers would fire spuriously and every
//! command would report `Provider { Timeout }` against a perfectly healthy
//! server. The Postgres plugin hit the same thing through `sqlx`'s acquire
//! timeout, which is what put `TimeControl` in the shared crate in the first
//! place.
//!
//! The cost is that the runners skip the two virtual-time fault simulations
//! (`SC-LEAD-006`, `SC-DISC-006`) themselves. Both force a *missed* renewal to
//! assert re-enrolment, which a healthy real backend never exhibits by waiting;
//! they map to fault injection, which this plugin has no harness for
//! (TESTING.md §8).
//!
//! # Which fixture each suite runs on, and why they differ
//!
//! | Suite | Fixture | Why |
//! |---|---|---|
//! | cache | `start_redis` | Stock Redis is the deployment this plugin is for; its `EventuallyConsistent` declaration is honest and no cache scenario depends on linearizability |
//! | lock | `start_redis_lock_only` | The standalone `RedisLockPlugin` — the same path `ClusterLockProvider::build_lock` takes in production (DESIGN.md §3.5) |
//! | leader | **`start_redis_durable`** | `CasBasedLeaderElectionBackend::new` is the strict constructor and *refuses* an `EventuallyConsistent` cache, so on the default fixture this suite would fail to construct rather than fail a scenario |
//!
//! The leader row is the one wrinkle no other backend has, and it is deliberately
//! *not* worked around with the `allow_weak_consistency` opt-in. That flag
//! makes weak-cache leader election **expressible by an operator who opts in**,
//! not correct — and this suite asserts correctness properties like
//! single-leader-among-contenders. Pointing it at a weak cache via the flag would
//! produce a suite asserting guarantees the backend does not claim: a test that
//! fails for the right reason at the wrong layer. The flag's own path is
//! `RD-SPEC-004b`, and the negative half — that the strict constructor really does
//! refuse the ordinary container's cache — is `RD-SPEC-004`, which is the more
//! important of the two.
//!
//! # One container per suite, one backend per scenario
//!
//! Each suite starts a single container and builds a **fresh plugin instance per
//! scenario** over it, isolated into its own logical database *and* its own
//! `key_prefix` (`common::cluster_config_for_scenario`). A shared backend across
//! scenarios is not viable: the scenarios reuse key and lock names, and this
//! plugin's `stop()` deliberately *leaves* held leases to expire rather than
//! handing them back (`cpt-cf-clst-fr-shutdown-ttl-cleanup`), so a lease held past
//! one scenario's teardown would still be held when the next asked for the same
//! name.
//!
//! # Each suite is `Box::pin`ned
//!
//! A `run_*_conformance` future holds every scenario body in the suite, so it is
//! ~20 KB and trips clippy's `large_futures` (denied workspace-wide via
//! `pedantic`). Boxing is the right answer rather than an `allow`: these run once
//! per suite, so one heap allocation is free, and a 20 KB future living on the
//! stack of a test thread is worth avoiding on its own merits.
//!
//! Every `ScenarioBackend` is built with
//! [`ScenarioBackend::with_teardown`](cluster_conformance::ScenarioBackend::with_teardown)
//! and its teardown owns the handle and `stop()`s it. That is mandatory rather
//! than tidy: `RedisClusterHandle` and `RedisLockHandle` both **panic on drop**
//! without `stop()` in a debug build (ADR-006), which is what a test build is — so
//! a factory that dropped a handle would abort the process instead of failing a
//! test. `run_scenario` awaits the teardown even when a scenario assertion panics,
//! so this holds on the failure path too.

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a setup failure IS the test failure"
)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cluster_conformance::{ScenarioBackend, TimeControl};
use redis_cluster_plugin::{RedisClusterPlugin, RedisLockPlugin};

/// Every `SC-CACHE-*` scenario against a fresh Redis-backed cache per scenario.
///
/// The cache is taken through `handle.cache()`, so what the suite exercises is
/// the same `InstrumentedCache`-wrapped backend the wiring hands a consumer — not
/// the bare `RedisCache` underneath. That is deliberate: the decorator sits on
/// every call in production, and a contract violation it introduced (a swallowed
/// error, a mangled `CacheWatch`) would otherwise be invisible to the suite that
/// exists to catch contract violations.
#[tokio::test]
async fn cache_conformance() {
    use cluster_conformance::run_cache_conformance;

    let (_container, base_config) = common::start_redis().await;
    let url = base_config.url;
    let scenario_index = AtomicUsize::new(0);

    Box::pin(run_cache_conformance(
        || {
            let url = url.clone();
            let index = scenario_index.fetch_add(1, Ordering::Relaxed);
            async move {
                let config = common::cluster_config_for_scenario(
                    &url,
                    "confcache",
                    index,
                    serde_json::json!({}),
                );
                let handle = RedisClusterPlugin::builder(config)
                    .build_and_start()
                    .await
                    .expect("a fresh per-scenario instance starts against the test container");
                let cache = handle.cache();
                ScenarioBackend::with_teardown(cache, async move { handle.stop().await })
            }
        },
        TimeControl::Real,
    ))
    .await;
}

/// Every `SC-LOCK-*` scenario against the **standalone** `RedisLockPlugin`.
///
/// The standalone shape rather than the combined plugin's `handle.lock()`, and
/// this is the shape the runner's signature already implies: it hands the factory
/// no cache (`F: Fn() -> Fut`), which matches this plugin exactly. The native lock
/// builds its own pool and never rides a cache, so there is no shared-pool
/// shortcut being skipped here — `RedisLockProvider::build_lock` takes this same
/// path in production (DESIGN.md §3.5).
///
/// The combined plugin's lock is the *same* `RedisLock` over a shared pool, and
/// `RD-LOCK-008` is what covers that half end to end through real wiring.
#[tokio::test]
async fn lock_conformance() {
    use cluster_conformance::run_lock_conformance;

    let (_container, base_config) = common::start_redis_lock_only().await;
    let url = base_config.url;
    let scenario_index = AtomicUsize::new(0);

    Box::pin(run_lock_conformance(
        || {
            let url = url.clone();
            let index = scenario_index.fetch_add(1, Ordering::Relaxed);
            async move {
                let config = common::lock_config_for_scenario(
                    &url,
                    "conflock",
                    index,
                    serde_json::json!({}),
                );
                let handle = RedisLockPlugin::builder(config)
                    .build_and_start()
                    .await
                    .expect("a fresh per-scenario standalone lock instance starts");
                let lock = handle.lock();
                ScenarioBackend::with_teardown(lock, async move { handle.stop().await })
            }
        },
        TimeControl::Real,
    ))
    .await;
}

/// The `SC-LEAD-*` scenarios against `CasBasedLeaderElectionBackend` over this
/// plugin's cache — the SDK default, which is the only way leader election is ever
/// served on Redis (DESIGN.md §7: there is no native implementation and no
/// `ClusterLeaderElectionProvider` registered for `redis`).
///
/// **On `start_redis_durable`, not the default fixture.** `::new` is the strict
/// constructor: it rejects any cache declaring `EventuallyConsistent`, so this
/// factory would return `Err` on every scenario against stock Redis. The durable
/// single node (`appendonly yes`, `appendfsync always`, no replicas) is the one
/// configuration ADR-009 rates safe, so its cache declares `Linearizable` and the
/// constructor accepts it — which also means the `expect` below is a real
/// assertion, not a formality: it is what would catch the consistency declaration
/// silently weakening on this fixture.
///
/// `run_leader_conformance` skips `SC-LEAD-006` itself under `Real` — see this
/// module's header on why a forced renewal *miss* is fault injection rather than a
/// long sleep.
#[tokio::test]
async fn leader_conformance() {
    use cluster::defaults::CasBasedLeaderElectionBackend;
    use cluster_conformance::run_leader_conformance;
    use cluster_sdk::LeaderElectionBackend;

    let (_container, base_config) = common::start_redis_durable().await;
    let url = base_config.url;
    let scenario_index = AtomicUsize::new(0);

    Box::pin(run_leader_conformance(
        || {
            let url = url.clone();
            let index = scenario_index.fetch_add(1, Ordering::Relaxed);
            async move {
                let config = common::cluster_config_for_scenario(
                    &url,
                    "confleader",
                    index,
                    serde_json::json!({}),
                );
                let handle = RedisClusterPlugin::builder(config)
                    .build_and_start()
                    .await
                    .expect("a fresh per-scenario instance starts against the durable container");
                let leader = Arc::new(CasBasedLeaderElectionBackend::new(handle.cache()).expect(
                    "the durable single-node fixture's cache must declare Linearizable, or the \
                         strict leader constructor refuses it and this suite cannot run at all \
                         (TESTING.md sec 3, RD-SPEC-003)",
                )) as Arc<dyn LeaderElectionBackend>;
                ScenarioBackend::with_teardown(leader, async move { handle.stop().await })
            }
        },
        TimeControl::Real,
    ))
    .await;
}
