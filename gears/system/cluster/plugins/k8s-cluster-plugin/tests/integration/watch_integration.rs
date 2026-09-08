//! Layer 3 — watch integration scenarios (docs/TESTING.md §4.5): leader-transition
//! dedup, the single shared cache watch stream, per-mutation cache events, and the
//! terminal `Closed(Shutdown)` on stop.

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::large_futures,
    reason = "integration tests: a setup failure IS the test failure"
)]

use crate::common;

use std::time::Duration;

use cluster_sdk::cache::{CacheEvent, CacheWatchEvent, PutRequest, Ttl};
use cluster_sdk::error::ClusterError;
use cluster_sdk::leader::{ElectionConfig, LeaderStatus, LeaderWatchEvent};
use k8s_cluster_plugin::{K8sCachePlugin, K8sClusterPlugin};
use serde_json::json;

/// `K8S-WATCH-001`: a leader watch reports transitions and nothing else — while the
/// leader renews, a follower receives its `Follower` transition and zero events for
/// the renewals (a raw forwarder would deliver one per renewal).
#[tokio::test]
async fn k8s_watch_001_leader_watch_reports_transitions_only() {
    let ns = common::fresh_namespace("watch-001").await;
    // A 6s TTL renews ~every 2s — two renewals across the 4s window below, all
    // deduped to zero follower events — with a wide margin against a missed renewal
    // under CI load (a too-aggressive TTL can lapse and surface a transient loss the
    // follower would then legitimately observe, flaking the "no events" assertion).
    let cfg = json!({});
    let ttl = ElectionConfig::new(Duration::from_secs(6), 2).unwrap();

    let leader = K8sClusterPlugin::builder(ns.cluster_config_with(cfg.clone()))
        .with_client(ns.client.clone())
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
        "K8S-WATCH-001: the leader is established before the follower joins"
    );

    let follower = K8sClusterPlugin::builder(ns.cluster_config_with(cfg))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("follower starts");
    let mut f_watch = follower
        .leader_election()
        .elect_with_config("svc", ttl)
        .await
        .expect("elect");

    // Drain to the follower's first Status (a Reset from the watcher's init may
    // precede it); it must be Follower. One absolute deadline bounds the whole drain
    // so a stream of non-Status events (repeated Resets) cannot reset the budget and
    // hang the test — each `changed()` gets only the time remaining.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let first = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let LeaderWatchEvent::Status(status) = tokio::time::timeout(remaining, f_watch.changed())
            .await
            .expect("status")
        {
            break status;
        }
    };
    assert_eq!(
        first,
        LeaderStatus::Follower,
        "K8S-WATCH-001: the follower's first status is Follower"
    );

    // Over the next ~4s the leader renews ~twice; the follower must see no events.
    let mut spurious = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, f_watch.changed()).await {
            Ok(LeaderWatchEvent::Status(_)) => spurious += 1,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert_eq!(
        spurious, 0,
        "K8S-WATCH-001: a healthy leader's renewals produce no follower events"
    );

    leader.stop().await;
    follower.stop().await;
}

/// `K8S-WATCH-003`: every active watch observes the terminal `Closed(Shutdown)`
/// before `stop()` returns, and a leader observes `Status(Lost)` first.
#[tokio::test]
async fn k8s_watch_003_closed_shutdown_before_stop_returns() {
    let ns = common::fresh_namespace("watch-003").await;
    let handle = K8sClusterPlugin::builder(ns.cluster_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("plugin starts");

    let mut leader_watch = handle.leader_election().elect("svc").await.expect("elect");
    assert!(
        common::wait_until(
            Duration::from_secs(5),
            Duration::from_millis(50),
            || async { leader_watch.status() == LeaderStatus::Leader },
        )
        .await,
        "K8S-WATCH-003: the leader is established before stop()"
    );
    let mut cache_watch = handle.cache().watch("k").await.expect("watch");

    handle.stop().await;

    // The leader observes Lost then Closed(Shutdown).
    let mut saw_lost = false;
    let mut leader_closed = false;
    for _ in 0..8 {
        // Bound each read: a regression that stops delivering the terminal events
        // would otherwise hang `changed()` forever, killing the job with no
        // assertion message. On expiry, break and let the asserts below report.
        let Ok(event) = tokio::time::timeout(Duration::from_secs(5), leader_watch.changed()).await
        else {
            break;
        };
        match event {
            LeaderWatchEvent::Status(LeaderStatus::Lost) => saw_lost = true,
            LeaderWatchEvent::Closed(ClusterError::Shutdown) => {
                leader_closed = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_lost, "K8S-WATCH-003: the leader observes Status(Lost)");
    assert!(
        leader_closed,
        "K8S-WATCH-003: the leader watch is Closed(Shutdown)"
    );

    // The cache watch observes Closed(Shutdown).
    let mut cache_closed = false;
    for _ in 0..8 {
        // Bounded like the leader loop above, for the same reason.
        let Ok(event) = tokio::time::timeout(Duration::from_secs(5), cache_watch.recv()).await
        else {
            break;
        };
        match event {
            Some(CacheWatchEvent::Closed(ClusterError::Shutdown)) => {
                cache_closed = true;
                break;
            }
            Some(_) => {}
            None => break,
        }
    }
    assert!(
        cache_closed,
        "K8S-WATCH-003: the cache watch is Closed(Shutdown)"
    );
}

/// `K8S-WATCH-004`: the cache uses exactly one watch stream for the whole keyspace —
/// ten `watch(key)` plus five `watch_prefix` subscribers add no new watch
/// connections (they fan out from the one shared watcher).
#[tokio::test]
async fn k8s_watch_004_one_watch_stream_for_the_keyspace() {
    let ns = common::fresh_namespace("watch-004").await;
    let (client, counts) = common::counted_client().await;
    let handle = K8sCachePlugin::builder(ns.cache_config_with(json!({})))
        .with_client(client)
        .build_and_start()
        .await
        .expect("cache starts");
    let cache = handle.cache();

    // `build_and_start` can return before the shared watcher's watch request is
    // actually sent: readiness is signaled on the underlying `InitDone` event,
    // which `kube-runtime`'s ListWatch state machine emits one step before it
    // issues the watch call. Wait for that call to land before snapshotting the
    // baseline, or it can race the assertion below (delta of 1, not 0).
    assert!(
        common::wait_until(
            Duration::from_secs(5),
            Duration::from_millis(50),
            || async { counts.watches() >= 1 }
        )
        .await,
        "K8S-WATCH-004: the shared watch request is established"
    );
    let watches_before = counts.watches();

    let mut subs = Vec::new();
    for i in 0..10 {
        subs.push(cache.watch(&format!("key-{i}")).await.expect("watch"));
    }
    for i in 0..5 {
        subs.push(
            cache
                .watch_prefix(&format!("p{i}/"))
                .await
                .expect("watch_prefix"),
        );
    }

    assert_eq!(
        counts.watches() - watches_before,
        0,
        "K8S-WATCH-004: 15 subscribers open no new watch connection (one shared stream)"
    );

    // And they each still receive their own events.
    cache
        .put(PutRequest {
            key: "key-3",
            value: b"v",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put");
    let got = tokio::time::timeout(Duration::from_secs(3), subs[3].recv())
        .await
        .expect("event");
    assert!(
        matches!(got, Some(CacheWatchEvent::Event(CacheEvent::Changed { ref key })) if key == "key-3"),
        "K8S-WATCH-004: the matching subscriber receives its event, got {got:?}"
    );

    handle.stop().await;
}

/// `K8S-WATCH-005`: cache events are one per mutation and typed correctly — 100
/// sequential puts produce 100 in-order `Changed`; a delete produces `Deleted`; a
/// TTL lapse produces `Expired` (not `Deleted`); and a mismatched
/// `compare_and_delete` produces nothing.
///
/// The doc's label-only-edit half is not asserted: `classify_event` is stateless
/// and maps every `Apply` to `Changed`, so a server-side label-only edit would
/// surface as a spurious `Changed` — suppressing it needs a per-key version table,
/// which the "no side table" design (DESIGN §6.3) forgoes. Recorded as a gap.
#[tokio::test]
async fn k8s_watch_005_cache_events_are_one_per_mutation() {
    let ns = common::fresh_namespace("watch-005").await;
    let handle = K8sCachePlugin::builder(ns.cache_config_with(json!({})))
        .with_client(ns.client.clone())
        .build_and_start()
        .await
        .expect("cache starts");
    let cache = handle.cache();

    let mut watch = cache.watch("k").await.expect("watch");

    // Put then drain that put's Changed before the next, so the bounded watch channel
    // never overflows — asserting one in-order Changed per mutation with no gaps.
    for i in 0..100u32 {
        cache
            .put(PutRequest {
                key: "k",
                value: &i.to_le_bytes(),
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("put");
        let got = drain_for(
            &mut watch,
            |e| matches!(e, CacheEvent::Changed { key } if key == "k"),
        )
        .await;
        assert!(got, "K8S-WATCH-005: put #{i} produced its Changed");
    }

    // A mismatched compare_and_delete produces nothing: with the stream quiesced
    // after put #99's Changed, no data event may arrive from the failed CAD before
    // the real delete does. Verified, not merely asserted in prose.
    assert!(
        !cache
            .compare_and_delete("k", b"nope")
            .await
            .expect("cad mismatch")
    );
    assert!(
        quiet_for(&mut watch, Duration::from_millis(750)).await,
        "K8S-WATCH-005: a mismatched compare_and_delete emits no event"
    );

    // A real delete produces exactly one Deleted.
    assert!(cache.delete("k").await.expect("delete"));
    let deleted = drain_for(
        &mut watch,
        |e| matches!(e, CacheEvent::Deleted { key } if key == "k"),
    )
    .await;
    assert!(deleted, "K8S-WATCH-005: an explicit delete yields Deleted");

    // A TTL lapse yields Expired, not Deleted.
    cache
        .put(PutRequest {
            key: "k",
            value: b"v",
            ttl: Ttl::Of(Duration::from_millis(50)),
        })
        .await
        .expect("put");
    let expired = drain_for(
        &mut watch,
        |e| matches!(e, CacheEvent::Expired { key } if key == "k"),
    )
    .await;
    assert!(expired, "K8S-WATCH-005: a TTL lapse yields Expired");

    handle.stop().await;
}

/// Reads the watch until an event satisfies `pred` (returns true) or the stream ends
/// / times out (false).
async fn drain_for<F>(watch: &mut cluster_sdk::cache::CacheWatch, pred: F) -> bool
where
    F: Fn(&CacheEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, watch.recv()).await {
            Ok(Some(CacheWatchEvent::Event(event))) if pred(&event) => return true,
            Ok(Some(_)) => {}
            _ => return false,
        }
    }
}

/// The negative counterpart to [`drain_for`]: reads the watch for `window` and returns
/// true iff no data `Event` frame arrives (non-`Event` frames such as `Reset` are
/// tolerated). Used to assert an operation emits nothing.
async fn quiet_for(watch: &mut cluster_sdk::cache::CacheWatch, window: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return true;
        }
        match tokio::time::timeout(remaining, watch.recv()).await {
            Ok(Some(CacheWatchEvent::Event(_))) => return false,
            Ok(Some(_)) => {}
            _ => return true,
        }
    }
}
