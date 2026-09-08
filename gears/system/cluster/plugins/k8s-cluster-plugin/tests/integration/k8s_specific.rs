//! Layer 3 — Kubernetes-specific scenarios (docs/TESTING.md §4.7): the wire-format,
//! declaration, request-volume, and isolation assertions no conformance scenario can
//! reach.
//!
//! Deferred here and tracked: the end-to-end YAML routing scenarios
//! (K8S-SPEC-005/006/017) belong with the `cluster` gear's wiring suite and are
//! large integration set-ups; the clock-skew scenario (K8S-SPEC-007) needs an
//! injectable timestamp source the backend does not expose; and the RBAC-revocation
//! scenario (K8S-SPEC-008) needs a restricted `ServiceAccount`.
//!
//! `K8S-SPEC-016` (the full `OTel` catalog) is implemented below. It does **not**
//! assert `cluster_watch_resets` presence: that counter fires only from the SDK's
//! `RestartingWatch` on a reconnect-after-`Closed`, but the k8s cache/leader watchers
//! use `kube`'s internal re-listing, which delivers a `Reset` *event* without
//! terminally closing, so that path never triggers. Emitting it needs the plugin to
//! call `metrics.watch_reset(primitive)` at its own relist points (threading a sink
//! into `K8sCache`); flagged as the one remaining observability gap (Phase 7,
//! alongside the `K8S-FAULT-001` watch-loss scenario that would exercise it).

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::large_futures,
    reason = "integration tests: a setup failure IS the test failure"
)]

use crate::common;

use std::collections::BTreeSet;
use std::time::Duration;

use cluster_sdk::cache::{CacheConsistency, PutRequest, Ttl};
use cluster_sdk::leader::{ElectionConfig, LeaderStatus};
use k8s_cluster_plugin::{
    K8sCachePlugin, K8sClusterPlugin, K8sLeaderElectionPlugin, K8sLockPlugin,
};
use kube::ResourceExt;
use serde_json::json;

const ANNOTATION_NAME: &str = "cluster.cf-gears.io/name";

/// Whether `name` is a legal Kubernetes object name — an RFC 1123 *subdomain*: at
/// most 253 chars total, lowercase alphanumerics / `-` / `.`, each dot-separated
/// label non-empty and not `-`-bounded. (The 63-char-per-label cap is DNS *label*
/// validation, which k8s object-name subdomain validation does not impose.)
fn is_legal_object_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// `K8S-SPEC-001`: adversarial coordination names produce legal, distinct objects —
/// `/`-separated, uppercase, unicode, and a 4 KiB name all elect successfully; the
/// object names are legal RFC 1123 subdomains; `a/b` and `a-b` land on different
/// objects; and each object's name annotation round-trips the original exactly.
#[tokio::test]
async fn k8s_spec_001_adversarial_names_produce_legal_distinct_objects() {
    let ns = common::fresh_namespace("spec-001").await;
    let handle = K8sLeaderElectionPlugin::builder(ns.leader_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("leader starts");
    let leader = handle.leader_election();

    let big = "x".repeat(4096);
    let names = ["a/b", "a-b", "Shard/7", "caf\u{e9}/\u{3bb}", big.as_str()];
    let mut watches = Vec::new();
    for name in names {
        watches.push(leader.elect(name).await.expect("elect an adversarial name"));
    }
    // Let the objects land.
    assert!(
        common::wait_until(
            Duration::from_secs(5),
            Duration::from_millis(100),
            || async { ns.list_leases().await.len() >= names.len() },
        )
        .await,
        "K8S-SPEC-001: every elected object lands within the window"
    );

    let leases = ns.list_leases().await;
    let mut object_names = BTreeSet::new();
    let mut annotations = BTreeSet::new();
    for lease in &leases {
        let object_name = lease.name_any();
        assert!(
            is_legal_object_name(&object_name),
            "K8S-SPEC-001: {object_name} is not a legal RFC 1123 subdomain"
        );
        object_names.insert(object_name);
        annotations.insert(
            lease
                .annotations()
                .get(ANNOTATION_NAME)
                .cloned()
                .unwrap_or_default(),
        );
    }
    assert_eq!(
        object_names.len(),
        names.len(),
        "K8S-SPEC-001: distinct objects (a/b != a-b)"
    );
    for name in names {
        assert!(
            annotations.contains(name),
            "K8S-SPEC-001: the name annotation round-trips {name:?} exactly"
        );
    }

    handle.stop().await;
}

/// `K8S-SPEC-003`: `linearizable: true` is declared for both coordination
/// primitives with no configuration hint involved.
#[tokio::test]
async fn k8s_spec_003_linearizable_declared_for_both_primitives() {
    let ns = common::fresh_namespace("spec-003").await;
    let leader = K8sLeaderElectionPlugin::builder(ns.leader_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("leader starts");
    assert!(
        leader.leader_election().features().linearizable,
        "K8S-SPEC-003: leader election declares linearizable"
    );
    leader.stop().await;

    let lock = K8sLockPlugin::builder(ns.lock_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("lock starts");
    assert!(
        lock.lock().features().linearizable,
        "K8S-SPEC-003: the lock declares linearizable"
    );
    lock.stop().await;
}

/// `K8S-SPEC-004`: two instances with different `lease_prefix`es are fully isolated —
/// in one namespace, both take "the same" lock name simultaneously (different
/// objects), and neither is visible to the other across the reaper's list.
#[tokio::test]
async fn k8s_spec_004_distinct_prefixes_are_isolated() {
    let ns = common::fresh_namespace("spec-004").await;
    let a = K8sLockPlugin::builder(ns.lock_config_with(json!({ "lease_prefix": "alpha" })))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("A starts");
    let b = K8sLockPlugin::builder(ns.lock_config_with(json!({ "lease_prefix": "beta" })))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("B starts");

    // Both hold "res" at once: the prefixes map it to different objects.
    let _a_guard = a
        .lock()
        .try_lock("res", Duration::from_secs(30))
        .await
        .expect("A acquires res");
    let _b_guard = b
        .lock()
        .try_lock("res", Duration::from_secs(30))
        .await
        .expect("B acquires res too");

    let leases = ns.list_leases().await;
    assert_eq!(
        leases.len(),
        2,
        "K8S-SPEC-004: two distinct objects for the same coordination name"
    );

    a.stop().await;
    b.stop().await;
}

/// `K8S-SPEC-010`: a follower's request volume is bounded and known — over a window,
/// five election followers issue zero mutating requests and hold a watch, while the
/// leader issues the renewals.
#[tokio::test]
async fn k8s_spec_010_follower_request_volume_is_bounded() {
    let ns = common::fresh_namespace("spec-010").await;
    // A 6s TTL (2s renewal, 4s deadline margin) keeps the leader stable under CI
    // load/contention — a shorter TTL could lapse, a follower could then acquire and
    // issue the very writes this scenario asserts followers never make. The leader
    // still renews (~every 2s) within the 3s window below, so "the leader renews".
    let cfg = json!({});
    let ttl = ElectionConfig::new(Duration::from_secs(6), 2).unwrap();

    let (leader_client, leader_counts) = common::counted_client().await;
    let leader = K8sLeaderElectionPlugin::builder(ns.leader_config_with(cfg.clone()))
        .with_client(leader_client)
        .build_and_start()
        .await
        .expect("leader starts");
    let l_watch = leader
        .leader_election()
        .elect_with_config("svc", ttl)
        .await
        .expect("elect");
    assert!(
        common::wait_until(
            Duration::from_secs(5),
            Duration::from_millis(50),
            || async { l_watch.status() == LeaderStatus::Leader },
        )
        .await,
        "K8S-SPEC-010: the candidate becomes leader within the window"
    );

    // Five followers on counted clients.
    let mut followers = Vec::new();
    let mut follower_counts = Vec::new();
    for _ in 0..5 {
        let (client, counts) = common::counted_client().await;
        let f = K8sLeaderElectionPlugin::builder(ns.leader_config_with(cfg.clone()))
            .with_client(client)
            .build_and_start()
            .await
            .expect("follower starts");
        let w = f
            .leader_election()
            .elect_with_config("svc", ttl)
            .await
            .expect("elect");
        assert!(
            common::wait_until(
                Duration::from_secs(5),
                Duration::from_millis(50),
                || async { w.status() == LeaderStatus::Follower },
            )
            .await,
            "K8S-SPEC-010: the candidate settles as a follower within the window"
        );
        followers.push((f, w));
        follower_counts.push(counts);
    }

    // Snapshot after everyone has settled, then observe a renewal window.
    let leader_mut_before = leader_counts.mutating();
    let follower_mut_before: Vec<u64> = follower_counts.iter().map(|c| c.mutating()).collect();
    tokio::time::sleep(Duration::from_secs(3)).await;

    for (i, counts) in follower_counts.iter().enumerate() {
        assert_eq!(
            counts.mutating() - follower_mut_before[i],
            0,
            "K8S-SPEC-010: follower {i} issues zero mutating requests"
        );
        assert!(
            counts.watches() >= 1,
            "K8S-SPEC-010: follower {i} holds a watch"
        );
    }
    assert!(
        leader_counts.mutating() > leader_mut_before,
        "K8S-SPEC-010: the leader issues the renewals"
    );

    leader.stop().await;
    for (f, _w) in followers {
        f.stop().await;
    }
}

/// `K8S-SPEC-018`: two plugin instances in different namespaces share the one CRD but
/// share no data — identical keys resolve to each namespace's own value, and a
/// prefix scan never crosses.
#[tokio::test]
async fn k8s_spec_018_two_namespaces_share_crd_share_no_data() {
    let ns_a = common::fresh_namespace("spec-018a").await;
    let ns_b = common::fresh_namespace("spec-018b").await;

    let a = K8sCachePlugin::builder(ns_a.cache_config_with(json!({})))
        .with_client(ns_a.client.clone())
        .build_and_start()
        .await
        .expect("A starts");
    let b = K8sCachePlugin::builder(ns_b.cache_config_with(json!({})))
        .with_client(ns_b.client.clone())
        .build_and_start()
        .await
        .expect("B starts");

    a.cache()
        .put(PutRequest {
            key: "shared",
            value: b"from-a",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put a");
    b.cache()
        .put(PutRequest {
            key: "shared",
            value: b"from-b",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put b");

    assert_eq!(
        a.cache()
            .get("shared")
            .await
            .expect("get")
            .expect("present")
            .value,
        b"from-a"
    );
    assert_eq!(
        b.cache()
            .get("shared")
            .await
            .expect("get")
            .expect("present")
            .value,
        b"from-b"
    );

    // A's cache backend also declares Linearizable under the default quorum reads.
    assert_eq!(a.cache().consistency(), CacheConsistency::Linearizable);

    // A prefix scan in one namespace never sees the other's keys.
    let a_keys = a.cache().scan_prefix("").await.expect("scan a");
    assert_eq!(
        a_keys,
        vec!["shared".to_owned()],
        "K8S-SPEC-018: A sees only its own key"
    );

    a.stop().await;
    b.stop().await;
}

/// The bounded `op` label vocabulary (OBSERVABILITY.md §5): cache + lock facade ops.
const OP_VALUES: &[&str] = &[
    // cache
    "get",
    "put",
    "delete",
    "contains",
    "put_if_absent",
    "compare_and_swap",
    "compare_and_delete",
    "scan_prefix",
    "watch",
    "watch_prefix",
    // lock
    "try_lock",
    "lock",
    "renew",
    "release",
];

/// The bounded `result` label vocabulary (`cluster_sdk::observability::result`).
const RESULT_VALUES: &[&str] = &[
    "ok",
    "conflict",
    "contended",
    "timeout",
    "expired",
    "shutdown",
    "unsupported",
    "error",
];

/// Metric-label keys allowed on any series (`METRIC_LABEL_ALLOWLIST`).
const LABEL_ALLOWLIST: &[&str] = &[
    "provider",
    "op",
    "result",
    "transition",
    "kind",
    "primitive",
    "profile",
];

/// Flattens every `cluster_*` counter/histogram data point in the exporter into
/// `(metric_name, label_key, label_value)` triples, so a scenario can assert on the
/// emitted series and their labels.
fn metric_labels(
    exporter: &opentelemetry_sdk::metrics::InMemoryMetricExporter,
) -> Vec<(String, String, String)> {
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    let mut triples = Vec::new();
    let Ok(metrics) = exporter.get_finished_metrics() else {
        return triples;
    };
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                let name = metric.name().to_owned();
                match metric.data() {
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                        for dp in sum.data_points() {
                            for kv in dp.attributes() {
                                triples.push((
                                    name.clone(),
                                    kv.key.as_str().to_owned(),
                                    kv.value.as_str().into_owned(),
                                ));
                            }
                        }
                    }
                    AggregatedMetrics::F64(MetricData::Histogram(hist)) => {
                        for dp in hist.data_points() {
                            for kv in dp.attributes() {
                                triples.push((
                                    name.clone(),
                                    kv.key.as_str().to_owned(),
                                    kv.value.as_str().into_owned(),
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    triples
}

/// `K8S-SPEC-016`: the ADR-004 catalog is emitted by this plugin alone. Driving
/// every operation of all three native primitives through an in-process `OTel` reader,
/// the cache, **lock**, and leader signal families all appear with `provider = "k8s"`;
/// no unbounded key (a cache key / lock name / election name) appears as a metric
/// label (only `METRIC_LABEL_ALLOWLIST` keys do); and the `op` / `result` label values
/// stay inside their bounded sets. Load-bearing here because all three primitives are
/// native — nothing emits on this plugin's behalf (DESIGN §9). The lock family is the
/// signal this test exists to guard: it emitted nothing until the lock backend was
/// wired to the metrics sink.
#[tokio::test]
async fn k8s_spec_016_full_catalog_emitted_with_bounded_labels() {
    use cluster_sdk::ClusterMetrics;
    use cluster_sdk::observability::otel::OtelClusterMetrics;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
    use std::sync::Arc;

    // A test-scoped meter over an in-memory reader, injected into this plugin via
    // `with_metrics` — no process-global provider is touched, so a concurrently
    // running sibling test cannot pollute this reader.
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let meter = provider.meter("k8s-spec-016");
    let metrics: Arc<dyn ClusterMetrics> = Arc::new(OtelClusterMetrics::new(
        &meter,
        k8s_cluster_plugin::PROVIDER_NAME,
    ));

    let ns = common::fresh_namespace("spec-016").await;
    let handle = K8sClusterPlugin::builder(ns.cluster_config_with(json!({})))
        .with_client(ns.client.clone())
        .with_metrics(Arc::clone(&metrics))
        .build_and_start()
        .await
        .expect("combined plugin starts");

    // Drive every cache op.
    let cache = handle.cache();
    cache
        .put(PutRequest {
            key: "k",
            value: b"v",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put");
    let _ = cache.get("k").await.expect("get");
    let _ = cache.contains("k").await.expect("contains");
    let _pia = cache
        .put_if_absent(PutRequest {
            key: "k",
            value: b"v2",
            ttl: Ttl::Indefinite,
        })
        .await;
    let cur = cache.get("k").await.expect("get").expect("present");
    let _ = cache
        .compare_and_swap("k", cur.version, b"v3", Ttl::Indefinite)
        .await
        .expect("cas");
    let _ = cache.compare_and_delete("k", b"nope").await.expect("cad");
    let _w = cache.watch("k").await.expect("watch");
    let _wp = cache.watch_prefix("k").await.expect("watch_prefix");
    let _ = cache.scan_prefix("").await.expect("scan");
    let _ = cache.delete("k").await.expect("delete");

    // Drive every lock op: acquire, renew, release, plus a blocking lock().
    let lock = handle.lock();
    let guard = lock
        .try_lock("m", Duration::from_secs(30))
        .await
        .expect("try_lock");
    guard.renew(Duration::from_secs(30)).await.expect("renew");
    guard.release().await.expect("release");
    let g2 = lock
        .lock("m2", Duration::from_secs(30), Duration::from_secs(5))
        .await
        .expect("lock");
    g2.release().await.expect("release");

    // Drive leader elect + transition + resign.
    let leader = handle.leader_election();
    let watch = leader.elect("svc").await.expect("elect");
    assert!(
        common::wait_until(
            Duration::from_secs(5),
            Duration::from_millis(50),
            || async { watch.status() == LeaderStatus::Leader },
        )
        .await,
        "K8S-SPEC-016: the candidate becomes leader before resigning"
    );
    watch.resign().await.expect("resign");

    provider.force_flush().expect("flush");
    let triples = metric_labels(&exporter);
    assert!(
        !triples.is_empty(),
        "K8S-SPEC-016: some metrics were emitted"
    );

    let series: BTreeSet<&str> = triples.iter().map(|(n, _, _)| n.as_str()).collect();
    // The lock family is the signal this test guards; assert it alongside the rest.
    for expected in [
        "cluster_lock_ops",
        "cluster_lock_op_duration_seconds",
        "cluster_cache_ops",
        "cluster_cache_op_duration_seconds",
        "cluster_leader_transitions",
    ] {
        assert!(
            series.contains(expected),
            "K8S-SPEC-016: `{expected}` must be emitted; saw {series:?}"
        );
    }

    // No unbounded key as a metric label; op/result inside their bounded sets; and
    // every `provider` value is `k8s`.
    for (metric, key, value) in &triples {
        assert!(
            LABEL_ALLOWLIST.contains(&key.as_str()),
            "K8S-SPEC-016: `{key}` on `{metric}` is not an allowlisted metric label (value {value:?})"
        );
        match key.as_str() {
            "provider" => assert_eq!(value, "k8s", "K8S-SPEC-016: provider label is k8s"),
            "op" => assert!(
                OP_VALUES.contains(&value.as_str()),
                "K8S-SPEC-016: op `{value}` is outside the bounded set"
            ),
            "result" => assert!(
                RESULT_VALUES.contains(&value.as_str()),
                "K8S-SPEC-016: result `{value}` is outside the bounded set"
            ),
            _ => {}
        }
    }

    handle.stop().await;
    drop(ns);
}
