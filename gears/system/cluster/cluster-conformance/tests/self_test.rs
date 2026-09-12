// Created: 2026-06-24 by Constructor Tech
//! Self-test: the conformance suites must pass against the known-correct
//! [`MemCache`] fixture. This proves the cache suite's assertions are
//! satisfiable (it doesn't reject a correct backend).
//!
//! The `run_leader_conformance`/`run_lock_conformance`
//! suites are not self-tested here against a concrete backend: this crate is a
//! dependency of every plugin (so its real `[dependencies]` stay limited to
//! `cluster-sdk`, deliberately excluding the `cluster` gear's SDK-default
//! backends), and it has no in-crate implementation of `LeaderElectionBackend`
//! / `DistributedLockBackend` to dogfood them
//! against. They get their first real exercise once a plugin (including the
//! `cluster` gear's own `CasBasedLeaderElectionBackend`,
//! and `CasBasedDistributedLockBackend`,
//! already covered by `cluster/src/defaults/{leader,lock}_tests.rs`)
//! adopts them.

use std::sync::Arc;

use cluster_conformance::fixture::MemCache;
use cluster_conformance::{
    ScenarioBackend, TimeControl, run_cache_conformance, run_restart_conformance,
    run_watch_lifecycle_conformance,
};
use cluster_sdk::ClusterCacheBackend;

#[tokio::test]
async fn cache_suite_passes_against_memcache() {
    run_cache_conformance(
        || async {
            ScenarioBackend::bare(MemCache::linearizable() as Arc<dyn ClusterCacheBackend>)
        },
        TimeControl::Virtual,
    )
    .await;
}

#[tokio::test]
async fn cache_suite_passes_against_a_watchless_memcache() {
    // A watchless fixture (features().watch == false, watch() -> Unsupported)
    // must satisfy the suite: the exact-watch scenarios (010/012/015) are
    // capability-gated, and SC-CACHE-012 asserts the Unsupported contract. This
    // proves a real watchless backend (e.g. redis watch_mode: disabled) passes
    // cache conformance rather than panicking on `.expect("watch")`.
    run_cache_conformance(
        || async {
            ScenarioBackend::bare(
                MemCache::linearizable_without_watch() as Arc<dyn ClusterCacheBackend>
            )
        },
        TimeControl::Virtual,
    )
    .await;
}

#[tokio::test]
async fn restart_suite_passes() {
    run_restart_conformance().await;
}

#[tokio::test]
async fn watch_lifecycle_suite_passes() {
    run_watch_lifecycle_conformance().await;
}
