//! Layer 3 — lifecycle integration scenarios (docs/TESTING.md §4.6): startup,
//! idempotency, config-vs-fault error classification, the ADR-006 Drop guard, and
//! the preflight canary's cleanup.
//!
//! The restricted-ServiceAccount scenarios (K8S-LIFE-004/005/006-denied) and the
//! missing/truncated-CRD scenarios (K8S-LIFE-012/013) live in `k8s_specific.rs`
//! alongside the other RBAC/wire assertions, and the container-pause shutdown
//! (K8S-LIFE-010) is deferred to L4 (Phase 7) since pausing the shared container
//! would break every concurrent scenario.

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::large_futures,
    reason = "integration tests: a setup failure IS the test failure"
)]

use crate::common;

use std::time::Duration;

use cluster_sdk::error::{ClusterError, ProviderErrorKind};
use k8s_cluster_plugin::{K8sCachePlugin, K8sClusterPlugin, K8sLockPlugin};
use kube::{Client, Config};
use serde_json::json;

/// `K8S-LIFE-001`: `build_and_start` authenticates, preflights, and reports `Ok`,
/// and startup creates no coordination objects (the cache canary cleans up after
/// itself).
#[tokio::test]
async fn k8s_life_001_build_and_start_creates_nothing() {
    let ns = common::fresh_namespace("life-001").await;
    let handle = K8sClusterPlugin::builder(ns.cluster_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("K8S-LIFE-001: build_and_start succeeds against the fixture");

    assert!(
        ns.list_leases().await.is_empty(),
        "K8S-LIFE-001: no Lease created at startup"
    );
    assert!(
        ns.list_cache_entries().await.is_empty(),
        "K8S-LIFE-001: no cache object created at startup (the canary cleaned up)"
    );

    handle.stop().await;
}

/// `K8S-LIFE-002`: `build_and_start` is idempotent and creates nothing — a second
/// start against the same namespace succeeds and the object inventory is unchanged.
#[tokio::test]
async fn k8s_life_002_build_and_start_is_idempotent() {
    let ns = common::fresh_namespace("life-002").await;
    let first = K8sClusterPlugin::builder(ns.cluster_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("first start");
    first.stop().await;

    let second = K8sClusterPlugin::builder(ns.cluster_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("K8S-LIFE-002: the second start also succeeds");
    assert!(
        ns.list_leases().await.is_empty(),
        "K8S-LIFE-002: inventory unchanged"
    );
    assert!(
        ns.list_cache_entries().await.is_empty(),
        "K8S-LIFE-002: inventory unchanged"
    );
    second.stop().await;
}

/// `K8S-LIFE-003`: an unresolvable namespace is a config error, not a fallback to
/// `default`. With no `namespace`, no `POD_NAMESPACE`, no SA file, and no kubeconfig
/// namespace, `build_and_start` returns `InvalidConfig` naming the missing sources.
#[tokio::test]
async fn k8s_life_003_unresolvable_namespace_is_a_config_error() {
    let ns = common::fresh_namespace("life-003").await;
    let client = ns.client.clone();
    // Point KUBECONFIG at nothing and clear POD_NAMESPACE so no source resolves.
    let outcome = temp_env::async_with_vars(
        [
            ("KUBECONFIG", Some("/nonexistent-kubeconfig")),
            ("POD_NAMESPACE", None),
        ],
        async {
            K8sClusterPlugin::builder(ns.cluster_config_with(json!({ "namespace": null })))
                .with_client(client)
                .build_and_start()
                .await
        },
    )
    .await;

    match outcome {
        Err(ClusterError::InvalidConfig { reason }) => {
            assert!(
                reason.contains("namespace"),
                "names the namespace: {reason}"
            );
            assert!(
                reason.contains("default"),
                "K8S-LIFE-003: states there is no default fallback"
            );
        }
        Err(other) => panic!("K8S-LIFE-003: expected InvalidConfig, got {other:?}"),
        Ok(_) => panic!("K8S-LIFE-003: an unresolvable namespace must not start"),
    }
}

/// `K8S-LIFE-006` (skip half): `skip_rbac_preflight: true` starts successfully **and**
/// issues no `SelfSubjectAccessReview` probe, contrasted with a non-zero probe count
/// when the flag is unset.
///
/// The lock plugin is used because its startup path issues no cache canary, so the
/// only mutating request at build time is the RBAC probe (a `SelfSubjectAccessReview`
/// create) — making `ApiCounts::mutating()` exactly the probe count.
#[tokio::test]
async fn k8s_life_006_skip_rbac_preflight_skips_the_probe() {
    let ns = common::fresh_namespace("life-006").await;

    // Flag set: startup succeeds and issues no RBAC probe.
    let (skip_client, skip_counts) = common::counted_client().await;
    let skipped =
        K8sLockPlugin::builder(ns.lock_config_with(json!({ "skip_rbac_preflight": true })))
            .with_client(skip_client)
            .build_and_start()
            .await
            .expect("K8S-LIFE-006: skip_rbac_preflight starts the plugin");
    assert_eq!(
        skip_counts.mutating(),
        0,
        "K8S-LIFE-006: with the flag set, no SelfSubjectAccessReview is issued"
    );
    skipped.stop().await;

    // Flag unset: the probe runs (a non-zero create count) and startup still succeeds.
    let (probe_client, probe_counts) = common::counted_client().await;
    let probed = K8sLockPlugin::builder(ns.lock_config_with(json!({})))
        .with_client(probe_client)
        .build_and_start()
        .await
        .expect("plugin starts with the probe");
    assert!(
        probe_counts.mutating() > 0,
        "K8S-LIFE-006: without the flag the RBAC probe runs (so the skip is doing something)"
    );
    probed.stop().await;
}

/// `K8S-LIFE-007`: an unreachable API server fails at startup with
/// `Provider { ConnectionLost }`, bounded, rather than hanging or returning `Ok`.
///
/// The cache plugin is used because its startup canary is a *hard* connection check
/// (a required create against a real object); the RBAC SSAR probe, by contrast,
/// degrades on any error to `rbac_unverified` (K8S-LIFE-006), so the lock/leader
/// plugins would treat an unreachable server as an unverifiable diagnostic and start.
#[tokio::test]
async fn k8s_life_007_unreachable_api_server_fails_bounded() {
    let ns = common::fresh_namespace("life-007").await;
    // A client pointed at a closed port; namespace is set so resolution passes and
    // the failure is the preflight canary's connection attempt.
    let config = Config::new("https://127.0.0.1:1".parse().unwrap());
    let dead_client = Client::try_from(config).expect("client builds");

    let start = tokio::time::Instant::now();
    let outcome =
        K8sCachePlugin::builder(ns.cache_config_with(json!({ "request_timeout_ms": 3000 })))
            .with_client(dead_client)
            .build_and_start()
            .await;
    match outcome {
        Err(ClusterError::Provider {
            kind: ProviderErrorKind::ConnectionLost,
            ..
        }) => {}
        Err(other) => panic!("K8S-LIFE-007: expected ConnectionLost, got {other:?}"),
        Ok(_) => panic!("K8S-LIFE-007: an unreachable server must not start"),
    }
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "K8S-LIFE-007: fails bounded, not hanging"
    );
}

/// `K8S-LIFE-008`: a handle dropped without `stop()` panics in a debug build with
/// the ADR-006 message; a `stop()`-then-drop does not.
#[cfg(debug_assertions)]
#[tokio::test]
async fn k8s_life_008_drop_without_stop_panics() {
    let ns = common::fresh_namespace("life-008").await;
    let handle = K8sLockPlugin::builder(ns.lock_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("lock starts");

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(handle)));
    let payload = panicked.expect_err("K8S-LIFE-008: dropping without stop() must panic in debug");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    assert!(
        message.contains("dropped without stop()"),
        "K8S-LIFE-008: the panic names the forgotten stop(), got {message:?}"
    );

    // stop()-then-drop is clean.
    let clean = K8sLockPlugin::builder(ns.lock_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("lock starts");
    clean.stop().await; // consumes + marks stopped; the subsequent drop is silent.
}

/// `K8S-LIFE-011`: a plugin built with a caller-supplied client (`with_client`)
/// works identically — the adoption path a host gear uses. All three primitives are
/// functional over the adopted client.
#[tokio::test]
async fn k8s_life_011_with_client_adopts_an_existing_client() {
    let ns = common::fresh_namespace("life-011").await;
    let adopted = ns.client.clone();
    let handle = K8sClusterPlugin::builder(ns.cluster_config_with(json!({})))
        .with_client(adopted)
        .build_and_start()
        .await
        .expect("K8S-LIFE-011: build_and_start adopts the supplied client");

    // The three primitives all work over the adopted client.
    handle
        .cache()
        .put(cluster_sdk::cache::PutRequest {
            key: "k",
            value: b"v",
            ttl: cluster_sdk::cache::Ttl::Indefinite,
        })
        .await
        .expect("cache works");
    assert!(handle.cache().get("k").await.expect("get").is_some());
    let _lock = handle
        .lock()
        .try_lock("res", Duration::from_secs(30))
        .await
        .expect("lock works");
    let _leader = handle
        .leader_election()
        .elect("svc")
        .await
        .expect("election works");

    handle.stop().await;
}

/// `K8S-LIFE-014`: the preflight canary leaves nothing behind — after a successful
/// cache `build_and_start`, no `<prefix>-ca-preflight-*` object exists.
#[tokio::test]
async fn k8s_life_014_canary_leaves_nothing_behind() {
    let ns = common::fresh_namespace("life-014").await;
    let handle = K8sCachePlugin::builder(ns.cache_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("cache starts");

    let leftover: Vec<String> = ns
        .list_cache_entries()
        .await
        .into_iter()
        .filter_map(|e| e.metadata.name)
        .filter(|n| n.contains("preflight"))
        .collect();
    assert!(
        leftover.is_empty(),
        "K8S-LIFE-014: the canary cleaned up, found {leftover:?}"
    );

    handle.stop().await;
}
