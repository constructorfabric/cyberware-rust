//! Layer 2 — the shared `cluster-conformance` suites run against this plugin's
//! three **native** primitives on a real k3s API server (docs/TESTING.md 3).
//!
//! This is the first plugin whose leader election is native rather than the SDK's
//! `CasBasedLeaderElectionBackend` over a cache, so `run_leader_conformance` here
//! exercises a genuinely independent implementation for the first time.
//!
//! Every suite runs under [`TimeControl::Real`]: a real API server and `kube`'s own
//! internal timers cannot run under a paused clock (the same reason the postgres
//! plugin uses `Real`). Per-scenario isolation is a fresh namespace; the scenario
//! owns its plugin handle and stops it via teardown before the next is built —
//! mandatory, since a handle panics on drop if never `stop()`ed (ADR-006).

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a setup failure IS the test failure"
)]
#![allow(
    clippy::large_futures,
    reason = "the shared conformance runners drive dozens of scenarios in one future; \
              its size is immaterial in a test binary and boxing every call site adds noise"
)]

use crate::common;

use cluster_conformance::{ScenarioBackend, TimeControl};
use k8s_cluster_plugin::{K8sCachePlugin, K8sLeaderElectionPlugin, K8sLockPlugin};
use serde_json::json;

#[tokio::test]
async fn cache_conformance() {
    use cluster_conformance::run_cache_conformance;

    run_cache_conformance(
        || async {
            let ns = common::fresh_namespace("conf-cache").await;
            let client = ns.client.clone();
            let handle = K8sCachePlugin::builder(ns.cache_config_with(json!({})))
                .with_client(client)
                .build_and_start()
                .await
                .expect("cache plugin starts against a fresh namespace");
            let cache = handle.cache();
            ScenarioBackend::with_teardown(cache, async move {
                handle.stop().await;
                drop(ns);
            })
        },
        TimeControl::Real,
    )
    .await;
}

/// The cache suite a second time under `cache_reads: cached`, where the declaration
/// flips to `EventuallyConsistent` and the capability gating inverts — the one place
/// this plugin lets an operator config move a declaration (docs/TESTING.md 3,
/// DESIGN.md 6.5), so both branches are exercised.
#[tokio::test]
async fn cache_conformance_cached() {
    use cluster_conformance::run_cache_conformance;

    run_cache_conformance(
        || async {
            let ns = common::fresh_namespace("conf-cache-cached").await;
            let client = ns.client.clone();
            let handle =
                K8sCachePlugin::builder(ns.cache_config_with(json!({ "cache_reads": "cached" })))
                    .with_client(client)
                    .build_and_start()
                    .await
                    .expect("cache plugin starts with cached reads");
            let cache = handle.cache();
            ScenarioBackend::with_teardown(cache, async move {
                handle.stop().await;
                drop(ns);
            })
        },
        TimeControl::Real,
    )
    .await;
}

#[tokio::test]
async fn lock_conformance() {
    use cluster_conformance::run_lock_conformance;

    run_lock_conformance(
        || async {
            let ns = common::fresh_namespace("conf-lock").await;
            let client = ns.client.clone();
            let handle = K8sLockPlugin::builder(ns.lock_config_with(json!({})))
                .with_client(client)
                .build_and_start()
                .await
                .expect("lock plugin starts against a fresh namespace");
            let lock = handle.lock();
            ScenarioBackend::with_teardown(lock, async move {
                handle.stop().await;
                drop(ns);
            })
        },
        TimeControl::Real,
    )
    .await;
}

/// The headline: `run_leader_conformance` against a **native** leader backend.
/// `SC-LEAD-006` (transient-loss re-enrol via fast-forwarded virtual time) is
/// skipped by the runner under `Real` and re-covered as `K8S-LEAD-008` (L3).
#[tokio::test]
async fn leader_conformance() {
    use cluster_conformance::run_leader_conformance;

    run_leader_conformance(
        || async {
            let ns = common::fresh_namespace("conf-leader").await;
            let client = ns.client.clone();
            // The shared suite's timing scenarios (e.g. SC-LEAD-003) use sub-second
            // election TTLs so they complete under `TimeControl::Real`'s 500ms elapse
            // cap; lower the floor so the plugin accepts them (TESTING §3).
            let handle = K8sLeaderElectionPlugin::builder(
                ns.leader_config_with(json!({ "min_election_ttl_ms": 50 })),
            )
            .with_client(client)
            .build_and_start()
            .await
            .expect("leader plugin starts against a fresh namespace");
            let leader = handle.leader_election();
            ScenarioBackend::with_teardown(leader, async move {
                handle.stop().await;
                drop(ns);
            })
        },
        TimeControl::Real,
    )
    .await;
}
