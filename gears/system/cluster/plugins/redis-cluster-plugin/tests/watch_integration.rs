//! Layer 3 — watch integration scenarios (docs/TESTING.md §4.4), `RD-WATCH-001`
//! through `RD-WATCH-010`.
//!
//! Watch is where this plugin diverges most from the other two backends, and where
//! the design decisions are least visible from the trait:
//!
//! - events are **published from inside the mutation script**, so one logical
//!   write is one event even though it is three Redis commands (`RD-WATCH-001`);
//! - `Expired` is the one event the plugin cannot publish for itself and comes
//!   from a server keyspace notification instead (`RD-WATCH-003`);
//! - prefix watch is **native** here — `PSUBSCRIBE`, not a polling polyfill — and
//!   N watchers on one prefix cost one server-side pattern (`RD-WATCH-004`,
//!   `RD-WATCH-005`). This is the first backend in the platform for which that is
//!   true;
//! - Redis pub/sub backpressure is ADR-003's canonical `Lagged` source, and
//!   `RD-WATCH-008` is **the first test in the platform that produces `Lagged` at
//!   all**.
//!
//! Every scenario drains its watch with a bounded `recv` rather than a bare
//! `.await`: a watch that never delivers is exactly the bug several of these look
//! for, and an unbounded await turns that into a hung test run rather than a
//! failure.

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a setup failure IS the test failure"
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use cluster_sdk::cache::{CacheEvent, CacheWatch, CacheWatchEvent, PutRequest, Ttl};
use cluster_sdk::{ClusterCacheBackend, ClusterError};
use fred::interfaces::PubsubInterface;
use redis_cluster_plugin::{RedisClusterHandle, RedisClusterPlugin};
use serde_json::json;

const VALUE: &[u8] = b"v";

/// How long a scenario waits for an event that should already be in flight.
///
/// Generous relative to a local pub/sub round trip (single-digit milliseconds) so
/// a loaded CI container does not fail a correctness assertion on timing, and
/// still short enough that a genuinely undelivered event fails the test in about a
/// second rather than hanging the run.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Starts a plugin over a stock container. Every scenario must `stop()` the
/// handle (ADR-006's `Drop` guard panics in a debug build).
async fn fixture(
    overrides: serde_json::Value,
) -> (
    testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>,
    RedisClusterHandle,
    Arc<dyn ClusterCacheBackend>,
    fred::clients::Client,
) {
    let (container, config) = common::start_redis_with(overrides).await;
    let url = config.url.clone();
    let database = config.database;
    let handle = RedisClusterPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the plugin starts against the test container");
    let cache = handle.cache();
    let raw = common::raw_client_on(&url, database).await;
    (container, handle, cache, raw)
}

/// Receives one event, or `None` if nothing arrives within [`DELIVERY_TIMEOUT`].
async fn next_event(watch: &mut CacheWatch) -> Option<CacheWatchEvent> {
    tokio::time::timeout(DELIVERY_TIMEOUT, watch.recv())
        .await
        .ok()
        .flatten()
}

/// Receives one event and asserts it is a `Changed`/`Deleted`/`Expired` for
/// `key`, returning it. Panics with what actually arrived, which is what makes a
/// failure diagnosable — "expected Changed, got Reset" says where to look.
async fn expect_event(watch: &mut CacheWatch, context: &str) -> CacheEvent {
    match next_event(watch).await {
        Some(CacheWatchEvent::Event(event)) => event,
        other => panic!("{context}: expected a cache event, got {other:?}"),
    }
}

/// Asserts nothing arrives within a short grace period.
///
/// Deliberately shorter than [`DELIVERY_TIMEOUT`]: this is proving a negative, and
/// every scenario that uses it pays the full wait on the happy path. A pub/sub
/// message that was going to arrive would have arrived in single-digit
/// milliseconds, so 300 ms is many times the margin needed.
async fn expect_silence(watch: &mut CacheWatch, context: &str) {
    let stray = tokio::time::timeout(Duration::from_millis(300), watch.recv()).await;
    assert!(
        stray.is_err(),
        "{context}: expected no further event, got {stray:?}"
    );
}

/// `RD-WATCH-001` — exactly **one** `Changed` per `put`, not three.
///
/// The assertion that holds the in-script publish design (DESIGN.md §4.3). A `put`
/// runs `HSET` + `HINCRBY` + `PEXPIRE`, so an implementation sourcing events from
/// raw keyspace notifications would deliver three events for one logical write —
/// and every consumer would re-read three times, which
/// `cpt-cf-clst-nfr-watch-delivery`'s no-duplicates requirement forbids.
///
/// The key is also covered by a prefix watch here, which is the sharper half: a
/// key watched both exactly and by prefix is subscribed twice server-side, so one
/// `PUBLISH` genuinely arrives twice on the connection. The registry routes
/// `message` only to exact watchers and `pmessage` only to prefix watchers, and
/// this is what pins that.
#[tokio::test]
async fn rd_watch_001_one_changed_per_put_even_when_doubly_covered() {
    let (_container, handle, cache, _raw) = fixture(json!({})).await;

    let mut exact = cache.watch("cov:key").await.expect("watch succeeds");
    let mut prefix = cache
        .watch_prefix("cov:")
        .await
        .expect("watch_prefix succeeds");

    cache
        .put(PutRequest {
            key: "cov:key",
            value: VALUE,
            ttl: Ttl::Of(Duration::from_secs(30)),
        })
        .await
        .expect("put succeeds");

    let event = expect_event(&mut exact, "RD-WATCH-001 exact").await;
    assert!(
        matches!(&event, CacheEvent::Changed { key } if key == "cov:key"),
        "one put must deliver exactly one Changed, got {event:?}"
    );
    expect_silence(
        &mut exact,
        "RD-WATCH-001: a put is HSET+HINCRBY+PEXPIRE, but one event",
    )
    .await;

    let event = expect_event(&mut prefix, "RD-WATCH-001 prefix").await;
    assert!(
        matches!(&event, CacheEvent::Changed { key } if key == "cov:key"),
        "the prefix watcher must also see exactly one Changed, got {event:?}"
    );
    expect_silence(
        &mut prefix,
        "RD-WATCH-001: a doubly-covered key arrives twice on the connection and must be routed \
         once to each family, not twice to both",
    )
    .await;

    handle.stop().await;
}

/// `RD-WATCH-002` — `Deleted` on both delete paths, and **nothing** on a
/// mismatched `compare_and_delete`.
///
/// The negative half is the interesting one: a `compare_and_delete` whose value
/// guard fails changed nothing, so publishing would tell every watcher to re-read
/// a key that is exactly as they last saw it. Because the publish is inside the
/// script, it sits on the same branch as the delete itself — which is what makes
/// "no change, no event" structural rather than a check someone remembered.
#[tokio::test]
async fn rd_watch_002_deleted_on_both_paths_and_nothing_on_a_mismatch() {
    let (_container, handle, cache, _raw) = fixture(json!({})).await;

    cache
        .put(PutRequest {
            key: "del:a",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put succeeds");
    let mut watch = cache.watch("del:a").await.expect("watch succeeds");
    assert!(cache.delete("del:a").await.expect("delete succeeds"));
    let event = expect_event(&mut watch, "RD-WATCH-002 delete").await;
    assert!(
        matches!(&event, CacheEvent::Deleted { key } if key == "del:a"),
        "delete must publish Deleted, got {event:?}"
    );

    cache
        .put(PutRequest {
            key: "del:b",
            value: b"mine",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put succeeds");
    let mut watch = cache.watch("del:b").await.expect("watch succeeds");

    assert!(
        !cache
            .compare_and_delete("del:b", b"not-mine")
            .await
            .expect("compare_and_delete succeeds"),
        "the guard does not match, so nothing is deleted"
    );
    expect_silence(
        &mut watch,
        "RD-WATCH-002: a mismatched compare_and_delete changed nothing and must publish nothing",
    )
    .await;

    assert!(
        cache
            .compare_and_delete("del:b", b"mine")
            .await
            .expect("compare_and_delete succeeds"),
        "the matching guard deletes"
    );
    let event = expect_event(&mut watch, "RD-WATCH-002 compare_and_delete").await;
    assert!(
        matches!(&event, CacheEvent::Deleted { key } if key == "del:b"),
        "a matching compare_and_delete must publish Deleted, got {event:?}"
    );

    handle.stop().await;
}

/// `RD-WATCH-003` — a TTL lapse delivers `Expired`, not `Deleted`.
///
/// The one event no plugin code can publish: nothing runs when a key expires, so
/// this arrives as a Redis `expired` keyspace notification and is the whole reason
/// the fixtures set `notify-keyspace-events Kxe` (DESIGN.md §4.3, TESTING.md §4.1,
/// 8 — `Kxe`, not TESTING §4.1's `Kgx$e`).
///
/// `Expired` rather than `Deleted` matters to a consumer: a TTL lapse is expected
/// and a delete is somebody's decision, and a cache-warming consumer would treat
/// them differently. Eviction maps the other way — to `Deleted`, since no TTL
/// elapsed — which is `RD-SPEC-007`.
#[tokio::test]
async fn rd_watch_003_a_ttl_lapse_delivers_expired() {
    let (_container, handle, cache, _raw) = fixture(json!({})).await;

    let mut watch = cache.watch("exp:key").await.expect("watch succeeds");
    cache
        .put(PutRequest {
            key: "exp:key",
            value: VALUE,
            ttl: Ttl::Of(Duration::from_millis(500)),
        })
        .await
        .expect("put succeeds");
    let created = expect_event(&mut watch, "RD-WATCH-003 put").await;
    assert!(matches!(created, CacheEvent::Changed { .. }));

    // Redis delivers `expired` when the key is actively reaped, which for a key
    // nobody touches is the background cycle rather than the deadline itself — so
    // this waits well past the TTL rather than exactly for it.
    let expired = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match watch.recv().await {
                Some(CacheWatchEvent::Event(CacheEvent::Expired { key })) => return Some(key),
                Some(CacheWatchEvent::Event(CacheEvent::Deleted { key })) => {
                    panic!(
                        "a TTL lapse must map to Expired, not Deleted - a consumer distinguishes \
                         'its time was up' from 'somebody removed it' ({key})"
                    )
                }
                Some(_other) => {}
                None => return None,
            }
        }
    })
    .await
    .ok()
    .flatten();
    assert_eq!(
        expired.as_deref(),
        Some("exp:key"),
        "a lapsed key must deliver Expired, sourced from the server's own keyspace notification"
    );

    handle.stop().await;
}

/// `RD-WATCH-004` — `watch_prefix` is native and delivers per-key events.
///
/// Native meaning `PSUBSCRIBE` rather than the SDK's `PollingPrefixWatch` polyfill
/// over `scan_prefix`. `features().prefix_watch` is asserted alongside the
/// behaviour because the declaration is what the SDK dispatches on: a backend that
/// delivered prefix events but declared `false` would silently get the polyfill,
/// and one that declared `true` without delivering would leave every service
/// discovery consumer watching a stream that never fires.
#[tokio::test]
async fn rd_watch_004_prefix_watch_is_native_and_per_key() {
    let (_container, handle, cache, _raw) = fixture(json!({})).await;

    assert!(
        cache.features().prefix_watch,
        "on a non-clustered server under watch_mode: publish, prefix watch is native"
    );

    let mut watch = cache
        .watch_prefix("tree:")
        .await
        .expect("watch_prefix succeeds");

    for key in ["tree:a", "tree:b", "tree:c"] {
        cache
            .put(PutRequest {
                key,
                value: VALUE,
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("put succeeds");
    }
    cache
        .put(PutRequest {
            key: "other:a",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("a put outside the prefix succeeds");

    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(
            expect_event(&mut watch, "RD-WATCH-004")
                .await
                .key()
                .to_owned(),
        );
    }
    seen.sort();
    assert_eq!(
        seen,
        vec![
            "tree:a".to_owned(),
            "tree:b".to_owned(),
            "tree:c".to_owned()
        ],
        "each key under the prefix produces its own event, naming that key"
    );
    expect_silence(
        &mut watch,
        "RD-WATCH-004: a write outside the prefix must not reach a prefix watcher",
    )
    .await;

    handle.stop().await;
}

/// `RD-WATCH-005` — five watchers on one prefix cost **one** Redis pattern.
///
/// `PUBSUB NUMPAT` is read from a separate connection, so it reports the server's
/// view rather than the plugin's belief about it. The in-process fan-out claim
/// (DESIGN.md §4.3) is not an optimisation detail: a pattern per watcher would put
/// the server's delivery cost in proportion to how many consumers a deployment
/// happens to have, on a broadcast every one of them then filters.
///
/// All five receiving every event is the other half — deduplicating subscriptions
/// is only correct if the fan-out actually fans out.
#[tokio::test]
async fn rd_watch_005_five_prefix_watchers_cost_one_pattern() {
    let (_container, handle, cache, raw) = fixture(json!({})).await;

    // The plugin's own always-on patterns (the keyspace family and the lock-release
    // family) are already subscribed, so the assertion is on the *increase*.
    let before: u64 = raw.pubsub_numpat().await.expect("PUBSUB NUMPAT succeeds");

    let mut watchers = Vec::new();
    for _ in 0..5 {
        watchers.push(
            cache
                .watch_prefix("fan:")
                .await
                .expect("watch_prefix succeeds"),
        );
    }

    let after: u64 = raw.pubsub_numpat().await.expect("PUBSUB NUMPAT succeeds");
    assert_eq!(
        after - before,
        1,
        "five watchers on one prefix must cost exactly one server-side pattern, not five \
         (before {before}, after {after})"
    );

    cache
        .put(PutRequest {
            key: "fan:key",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put succeeds");

    for (index, watch) in watchers.iter_mut().enumerate() {
        let event = expect_event(watch, &format!("RD-WATCH-005 watcher {index}")).await;
        assert_eq!(
            event.key(),
            "fan:key",
            "watcher {index} must receive the event the shared pattern carried"
        );
    }

    handle.stop().await;
}

/// `RD-WATCH-006` — per-key ordering is preserved across 100 sequential writes.
///
/// `cpt-cf-clst-nfr-watch-delivery` requires per-key ordering with no gaps. The
/// count is what makes this more than a smoke test: a fan-out that dispatched each
/// message onto its own task, or that used an unordered map iteration to route,
/// would pass a three-event version of this and reorder here.
///
/// The buffer is 64 slots, so 100 writes *would* lag a watcher that stopped
/// draining — this one drains as it goes, which is exactly the difference between
/// this scenario and `RD-WATCH-008`.
#[tokio::test]
async fn rd_watch_006_per_key_ordering_is_preserved() {
    let (_container, handle, cache, _raw) = fixture(json!({})).await;

    let mut watch = cache.watch("seq:key").await.expect("watch succeeds");

    // Drain concurrently with the writes: the channel holds 64 and 100 are coming.
    let drainer = tokio::spawn(async move {
        let mut received = 0_u32;
        let mut lagged = false;
        while received < 100 {
            match tokio::time::timeout(DELIVERY_TIMEOUT, watch.recv()).await {
                Ok(Some(CacheWatchEvent::Event(CacheEvent::Changed { .. }))) => received += 1,
                Ok(Some(CacheWatchEvent::Lagged { .. })) => lagged = true,
                Ok(Some(_other)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        (received, lagged)
    });

    for index in 0..100_u32 {
        cache
            .put(PutRequest {
                key: "seq:key",
                value: index.to_string().as_bytes(),
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("put succeeds");
    }

    let (received, lagged) = drainer.await.expect("the drainer task does not panic");
    assert!(
        !lagged,
        "a watcher draining as fast as one sequential writer produces must not lag"
    );
    assert_eq!(
        received, 100,
        "all 100 sequential writes must be delivered with no gaps"
    );

    handle.stop().await;
}

/// `RD-WATCH-007` — no cross-key delivery, and watchers on one key are
/// independent.
///
/// Two properties that a single shared broadcast channel would get wrong in
/// opposite directions: routing everything to everyone (the cross-key half), or
/// letting one consumer's disappearance take the others' subscription with it (the
/// independence half). The second is the reference-counted-subscription behaviour
/// registry does — `UNSUBSCRIBE` only when the *last* watcher on a key is pruned.
#[tokio::test]
async fn rd_watch_007_no_cross_key_delivery_and_independent_watchers() {
    let (_container, handle, cache, _raw) = fixture(json!({})).await;

    let mut on_a = cache.watch("iso:a").await.expect("watch succeeds");
    let mut first_on_b = cache.watch("iso:b").await.expect("watch succeeds");
    let mut second_on_b = cache.watch("iso:b").await.expect("watch succeeds");

    cache
        .put(PutRequest {
            key: "iso:b",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put succeeds");

    assert_eq!(
        expect_event(&mut first_on_b, "RD-WATCH-007 first")
            .await
            .key(),
        "iso:b"
    );
    assert_eq!(
        expect_event(&mut second_on_b, "RD-WATCH-007 second")
            .await
            .key(),
        "iso:b",
        "two watchers on one key must both receive the event"
    );
    expect_silence(
        &mut on_a,
        "RD-WATCH-007: a watcher on `iso:a` must see nothing when `iso:b` is written",
    )
    .await;

    // Dropping one watcher must not disturb its sibling's subscription.
    drop(first_on_b);
    cache
        .put(PutRequest {
            key: "iso:b",
            value: VALUE,
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put succeeds");
    assert_eq!(
        expect_event(&mut second_on_b, "RD-WATCH-007 survivor")
            .await
            .key(),
        "iso:b",
        "the surviving watcher keeps its subscription when a sibling is dropped - the \
         UNSUBSCRIBE is reference-counted and fires only on the last one"
    );

    handle.stop().await;
}

/// `RD-WATCH-008` — buffer overflow produces `Lagged`, **once**, with a count that
/// the drop counter agrees with.
///
/// **The first test in the platform that produces `Lagged` at all**, and the
/// reason ADR-003's variant is not dead weight: Redis pub/sub backpressure is the
/// canonical source, and neither the Postgres nor the standalone backend can drop
/// events under load the way this one can.
///
/// Coalesced into *one* `Lagged` rather than one per dropped message is the
/// assertion that matters. A consumer's response to `Lagged` is to re-read every
/// watched key; one per drop would turn a burst into thousands of re-reads and
/// make the backpressure worse than what caused it.
///
/// The known limitation (DESIGN.md §4.3): `Lagged` can only
/// ride the *next successful send* to that watcher — nothing polls a drained
/// buffer — so this scenario keeps writing after the watcher resumes.
#[tokio::test]
async fn rd_watch_008_overflow_produces_one_lagged_agreeing_with_the_counter() {
    let (meter, metrics) = common::in_memory_meter();
    let (_container, config) = common::start_redis().await;
    let handle = RedisClusterPlugin::builder(config)
        .__with_meter(meter)
        .build_and_start()
        .await
        .expect("the plugin starts against the test container");
    let cache = handle.cache();

    let mut watch = cache.watch("lag:key").await.expect("watch succeeds");

    // The watch buffer is 64 slots and nothing is draining it, so most of these
    // are dropped.
    for index in 0..600_u32 {
        cache
            .put(PutRequest {
                key: "lag:key",
                value: index.to_string().as_bytes(),
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("put succeeds");
    }

    // Drain what buffered, counting the coalesced `Lagged`. More writes follow
    // inside the loop because a `Lagged` rides the next successful send: a watcher
    // that lagged and then saw no further traffic on its key would never learn it
    // lagged at all.
    let mut lagged_events = 0_u32;
    let mut dropped_reported = 0_u64;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && lagged_events == 0 {
        match tokio::time::timeout(Duration::from_millis(200), watch.recv()).await {
            Ok(Some(CacheWatchEvent::Lagged { dropped })) => {
                lagged_events += 1;
                dropped_reported = dropped;
            }
            Ok(Some(_other)) => {}
            Ok(None) => break,
            Err(_timeout) => {
                cache
                    .put(PutRequest {
                        key: "lag:key",
                        value: VALUE,
                        ttl: Ttl::Indefinite,
                    })
                    .await
                    .expect("put succeeds");
            }
        }
    }

    assert_eq!(
        lagged_events, 1,
        "the drops must coalesce into exactly one Lagged, not one per dropped message"
    );
    assert!(
        dropped_reported > 0,
        "the Lagged must carry how many events were lost, so a consumer knows this is a re-read \
         rather than a hiccup"
    );

    let counted = metrics.counter("cluster_redis_watch_events_dropped_total");
    assert!(
        counted >= dropped_reported,
        "cluster_redis_watch_events_dropped_total ({counted}) must account for every event the \
         consumer was told about ({dropped_reported}) - the counter is incremented per dropped \
         event, so an operator's dashboard and the consumer's Lagged describe the same incident"
    );

    handle.stop().await;
}

/// `RD-WATCH-009` — every active watch observes `Closed(Shutdown)` **before**
/// `stop()` returns.
///
/// `cpt-cf-clst-fr-shutdown-revoke`. The ordering is the contract: a consumer that
/// learned its watch was dead only *after* shutdown completed would have a window
/// in which it believed a stale view current, which is the whole failure mode the
/// terminal event exists to close. `stop()` dispatches the terminal broadcast
/// against the registry directly rather than through the fan-out task, so it does
/// not depend on that task having noticed the cancellation yet.
///
/// Both watch kinds are asserted, since they are separate maps in the registry and
/// a close that walked only one would leave the other's consumers hanging.
#[tokio::test]
async fn rd_watch_009_every_watch_is_closed_before_stop_returns() {
    let (_container, handle, cache, _raw) = fixture(json!({})).await;

    let mut exact = cache.watch("shut:key").await.expect("watch succeeds");
    let mut prefix = cache
        .watch_prefix("shut:")
        .await
        .expect("watch_prefix succeeds");

    handle.stop().await;

    // No timeout needed on the receives: `stop()` has already returned, so if the
    // terminal event were not already queued it would never arrive — but a bound
    // keeps a regression a failure rather than a hang.
    for (label, watch) in [("exact", &mut exact), ("prefix", &mut prefix)] {
        let event = tokio::time::timeout(Duration::from_millis(500), watch.recv())
            .await
            .unwrap_or_else(|_| panic!("the {label} watch must already hold its terminal event"));
        assert!(
            matches!(event, Some(CacheWatchEvent::Closed(ClusterError::Shutdown))),
            "the {label} watch must observe Closed(Shutdown) before stop() returns, got {event:?}"
        );
    }
}

/// `RD-WATCH-010` — `watch_mode: disabled` degrades honestly.
///
/// "Honestly" is the operative word and it has four parts: both watch entry points
/// answer `Unsupported` rather than returning a stream that never fires, the
/// declaration matches so the SDK dispatches accordingly, **no `PUBLISH` is issued
/// on the write path** so the mode actually saves what it claims to, and the cache
/// itself keeps working.
///
/// The `PUBLISH` check is the one that would catch a half-implemented mode: a
/// plugin that stopped delivering but kept publishing would look correct from every
/// consumer's side while still paying the cost the operator turned the mode on to
/// avoid.
///
/// The subscriber connection itself stays open in this mode, which is a
/// amendment worth not "fixing": it carries the lock-release wake as well as cache
/// events (DESIGN.md §3.3), so closing it would silently push every blocked
/// acquisition onto the heartbeat fallback.
#[tokio::test]
async fn rd_watch_010_disabled_mode_degrades_honestly() {
    let (_container, handle, cache, raw) = fixture(json!({ "watch_mode": "disabled" })).await;
    let baseline_publishes = common::command_calls(&raw, "publish").await;

    assert!(
        !cache.features().prefix_watch,
        "watch_mode: disabled must declare no native prefix watch, so the SDK falls back to \
         PollingPrefixWatch rather than opening a stream that never fires"
    );
    assert!(
        matches!(
            cache.watch("off:key").await,
            Err(ClusterError::Unsupported { feature: "watch" })
        ),
        "watch must answer Unsupported"
    );
    assert!(
        matches!(
            cache.watch_prefix("off:").await,
            Err(ClusterError::Unsupported {
                feature: "prefix_watch"
            })
        ),
        "watch_prefix must answer Unsupported"
    );

    // The cache still works, and scan_prefix — which the SD polyfill drives — still
    // enumerates, so a disabled-watch deployment keeps working service discovery.
    for key in ["off:a", "off:b"] {
        cache
            .put(PutRequest {
                key,
                value: VALUE,
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("put succeeds");
    }
    let mut found = cache
        .scan_prefix("off:")
        .await
        .expect("scan_prefix still works with watches disabled");
    found.sort();
    assert_eq!(found, vec!["off:a".to_owned(), "off:b".to_owned()]);
    assert_eq!(
        cache
            .get("off:a")
            .await
            .expect("get succeeds")
            .expect("the entry is present")
            .value,
        VALUE
    );

    assert_eq!(
        common::command_calls(&raw, "publish").await,
        baseline_publishes,
        "no PUBLISH may be issued on the write path in this mode - otherwise the deployment pays \
         the cost it turned the mode on to avoid, while looking correct from every consumer's side"
    );

    handle.stop().await;
}
