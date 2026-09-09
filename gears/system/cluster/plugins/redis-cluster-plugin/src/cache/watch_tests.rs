use futures_util::FutureExt;

use super::*;

/// A registry with no subscriber behind it.
///
/// Every subscription decision short-circuits on `None`, so registration and
/// fan-out run exactly as they do in production while nothing reaches a server.
/// That is the whole registry apart from the two `SUBSCRIBE`/`UNSUBSCRIBE`
/// round trips, which Layer 3 covers.
fn registry() -> Arc<WatchRegistry> {
    // A recording sink rather than the production one: the registry's own
    // `cluster_redis_watch_events_dropped_total` and its `cluster.provider.error`
    // on a refused `UNSUBSCRIBE` are then observable without a meter provider.
    WatchRegistry::new(None, crate::test_support::recording_signals().0)
}

fn names() -> ChannelNames {
    ChannelNames::new("cluster", 0)
}

async fn watch_key(registry: &WatchRegistry, key: &str) -> CacheWatch {
    registry
        .register_key(key, &names().channel_for_key(key))
        .await
        .expect("registration without a subscriber cannot fail")
}

async fn watch_prefix(registry: &WatchRegistry, prefix: &str) -> CacheWatch {
    registry
        .register_prefix(prefix, &names().pattern_for_prefix(prefix))
        .await
        .expect("registration without a subscriber cannot fail")
}

fn changed(key: &str) -> ParsedNotification {
    ParsedNotification::Changed {
        key: key.to_owned(),
    }
}

/// Drains what is buffered without awaiting, so a test can assert "nothing
/// further arrived" without risking a hang.
///
/// `CacheWatch` exposes only an async `recv`, so `now_or_never` is what turns it
/// into a poll — the same thing the Postgres plugin's watch tests do, and for
/// the same reason.
fn drain(watch: &mut CacheWatch) -> Vec<CacheWatchEvent> {
    let mut events = Vec::new();
    while let Some(Some(event)) = watch.recv().now_or_never() {
        events.push(event);
    }
    events
}

fn changed_keys(events: &[CacheWatchEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            CacheWatchEvent::Event(CacheEvent::Changed { key }) => Some(key.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Per-key fan-out.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_watcher_on_a_key_receives_its_event() {
    let registry = registry();
    let mut first = watch_key(&registry, "k").await;
    let mut second = watch_key(&registry, "k").await;
    let mut other = watch_key(&registry, "other").await;

    let orphaned = registry.dispatch(&changed("k"), WatcherKind::Exact);

    assert!(orphaned.is_empty());
    assert_eq!(changed_keys(&drain(&mut first)), vec!["k".to_owned()]);
    assert_eq!(changed_keys(&drain(&mut second)), vec!["k".to_owned()]);
    assert!(
        drain(&mut other).is_empty(),
        "a watcher on a different key must see nothing"
    );
}

#[tokio::test]
async fn two_watchers_on_one_key_cost_one_subscription() {
    let registry = registry();
    let _first = watch_key(&registry, "k").await;
    let _second = watch_key(&registry, "k").await;
    assert_eq!(registry.key_subscription_count(), 1);
}

#[tokio::test]
async fn five_prefix_watches_on_one_prefix_cost_one_pattern() {
    // `RD-WATCH-005`. `PSUBSCRIBE` matching runs per published message against
    // every registered pattern, so N consumers watching one prefix must not
    // register N patterns.
    let registry = registry();
    let _watches: Vec<CacheWatch> = {
        let mut held = Vec::new();
        for _ in 0..5 {
            held.push(watch_prefix(&registry, "tenant-1/").await);
        }
        held
    };
    assert_eq!(registry.prefix_pattern_count(), 1);

    let _other = watch_prefix(&registry, "tenant-2/").await;
    assert_eq!(registry.prefix_pattern_count(), 2);
}

#[tokio::test]
async fn a_prefix_watcher_receives_every_key_it_covers() {
    let registry = registry();
    let mut watch = watch_prefix(&registry, "tenant-1/").await;

    let _orphaned = registry.dispatch(&changed("tenant-1/a"), WatcherKind::Prefix);
    let _orphaned = registry.dispatch(&changed("tenant-1/b"), WatcherKind::Prefix);
    let _orphaned = registry.dispatch(&changed("tenant-2/a"), WatcherKind::Prefix);

    assert_eq!(
        changed_keys(&drain(&mut watch)),
        vec!["tenant-1/a".to_owned(), "tenant-1/b".to_owned()],
        "a key outside the prefix must not reach this watcher"
    );
}

#[tokio::test]
async fn overlapping_prefixes_each_receive_the_key() {
    let registry = registry();
    let mut broad = watch_prefix(&registry, "tenant-1/").await;
    let mut narrow = watch_prefix(&registry, "tenant-1/orders/").await;

    let _orphaned = registry.dispatch(&changed("tenant-1/orders/7"), WatcherKind::Prefix);

    assert_eq!(drain(&mut broad).len(), 1);
    assert_eq!(drain(&mut narrow).len(), 1);
}

#[tokio::test]
async fn a_key_covered_both_ways_still_sees_exactly_one_event_per_write() {
    // The routing rule the module docs set out, and the reason `RD-WATCH-001`
    // asserts one `Changed` per `put`: the server delivers a `message` for the
    // exact subscription *and* a `pmessage` for the covering pattern, so a
    // fan-out that routed both to both sets would double every write for any
    // watcher covered twice.
    let registry = registry();
    let mut exact = watch_key(&registry, "tenant-1/a").await;
    let mut prefixed = watch_prefix(&registry, "tenant-1/").await;

    // One write, two deliveries from Redis.
    let _orphaned = registry.dispatch(&changed("tenant-1/a"), WatcherKind::Exact);
    let _orphaned = registry.dispatch(&changed("tenant-1/a"), WatcherKind::Prefix);

    assert_eq!(
        drain(&mut exact).len(),
        1,
        "the exact watcher must not also receive the pattern delivery"
    );
    assert_eq!(
        drain(&mut prefixed).len(),
        1,
        "the prefix watcher must not also receive the exact delivery"
    );
}

#[tokio::test]
async fn a_keyspace_event_reaches_both_families_from_its_single_pattern() {
    // The keyspace family has one blanket pattern, so there is no twin delivery
    // to split — routing it to only one family would silently drop the other's
    // `Expired`.
    let registry = registry();
    let mut exact = watch_key(&registry, "tenant-1/a").await;
    let mut prefixed = watch_prefix(&registry, "tenant-1/").await;

    let expired = ParsedNotification::Expired {
        key: "tenant-1/a".to_owned(),
    };
    let _orphaned = registry.dispatch(&expired, WatcherKind::Both);

    for events in [drain(&mut exact), drain(&mut prefixed)] {
        assert!(matches!(
            events.as_slice(),
            [CacheWatchEvent::Event(CacheEvent::Expired { .. })]
        ));
    }
}

#[tokio::test]
async fn a_dropped_watcher_is_pruned_and_orphans_its_subscription() {
    let registry = registry();
    let watch = watch_key(&registry, "k").await;
    assert_eq!(registry.key_subscription_count(), 1);
    drop(watch);

    let orphaned = registry.dispatch(&changed("k"), WatcherKind::Exact);

    assert_eq!(
        orphaned,
        vec![Orphaned {
            kind: WatcherKind::Exact,
            target: "k".to_owned(),
        }],
        "the last watcher going away must hand its subscription back for teardown"
    );
    assert_eq!(registry.key_subscription_count(), 0);
}

#[tokio::test]
async fn a_surviving_watcher_keeps_the_subscription() {
    let registry = registry();
    let dropped = watch_key(&registry, "k").await;
    let mut kept = watch_key(&registry, "k").await;
    drop(dropped);

    let orphaned = registry.dispatch(&changed("k"), WatcherKind::Exact);

    assert!(
        orphaned.is_empty(),
        "one watcher remains, so nothing orphans"
    );
    assert_eq!(drain(&mut kept).len(), 1);
    assert_eq!(registry.key_subscription_count(), 1);
}

// ---------------------------------------------------------------------------
// Backpressure.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_full_buffer_drops_coalesces_and_reports_one_lagged() {
    let registry = registry();
    let mut watch = watch_key(&registry, "k").await;

    // Fill the 64-slot buffer, then overrun it by three.
    for _ in 0..(WATCH_BUFFER + 3) {
        let _orphaned = registry.dispatch(&changed("k"), WatcherKind::Exact);
    }
    // Drain everything buffered, freeing the whole buffer.
    let first_batch = drain(&mut watch);
    assert_eq!(first_batch.len(), WATCH_BUFFER);
    assert!(
        first_batch
            .iter()
            .all(|event| matches!(event, CacheWatchEvent::Event(_))),
        "the buffered events themselves are ordinary events"
    );

    // The `Lagged` rides the next successful delivery rather than appearing the
    // moment the buffer drains — nothing polls a drained buffer.
    let _orphaned = registry.dispatch(&changed("k"), WatcherKind::Exact);
    let second_batch = drain(&mut watch);
    assert!(
        matches!(
            second_batch.as_slice(),
            [
                CacheWatchEvent::Lagged { dropped: 3 },
                CacheWatchEvent::Event(_)
            ]
        ),
        "exactly one coalesced Lagged, carrying the total dropped, then the event: got \
         {second_batch:?}"
    );
}

#[tokio::test]
async fn the_dropped_counter_agrees_with_the_lagged_the_consumer_receives() {
    // `RD-WATCH-008`'s Layer 1 half. The two numbers are produced by different
    // mechanisms — the counter is incremented per failed `try_send`, the
    // `Lagged` is a coalesced total drained on the next success — so their
    // agreeing is a property rather than a tautology, and it is the property a
    // dashboard reading the counter is relying on.
    let (signals, readback) = crate::test_support::metered_signals();
    let registry = WatchRegistry::new(None, signals);
    let mut watch = watch_key(&registry, "k").await;

    for _ in 0..(WATCH_BUFFER + 7) {
        let _orphaned = registry.dispatch(&changed("k"), WatcherKind::Exact);
    }
    let _first_batch = drain(&mut watch);
    let _orphaned = registry.dispatch(&changed("k"), WatcherKind::Exact);
    let second_batch = drain(&mut watch);

    let Some(CacheWatchEvent::Lagged { dropped }) = second_batch.first() else {
        panic!("expected a coalesced Lagged first, got {second_batch:?}");
    };
    assert_eq!(*dropped, 7);
    assert_eq!(
        readback.counter("cluster_redis_watch_events_dropped_total"),
        *dropped
    );
}

#[tokio::test]
async fn a_slow_watcher_does_not_stall_a_fast_one() {
    // The fan-out never awaits a slow consumer: `try_send`, so one stalled
    // watcher costs itself events and costs everyone else nothing.
    let registry = registry();
    let _slow = watch_key(&registry, "k").await;
    let mut fast = watch_key(&registry, "k").await;

    for _ in 0..(WATCH_BUFFER + 10) {
        let _orphaned = registry.dispatch(&changed("k"), WatcherKind::Exact);
        // Keep the fast watcher's buffer clear.
        let _drained = drain(&mut fast);
    }
    // Reaching here at all is the assertion: a blocking fan-out would have
    // deadlocked on the slow watcher's full buffer.
    assert_eq!(registry.key_subscription_count(), 1);
}

// ---------------------------------------------------------------------------
// Reset and the terminal close.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unintelligible_payload_resets_only_that_key() {
    // DESIGN.md §2.5: broadcast to every watcher *on that key*, not the whole
    // registry — one bad message is not evidence the subscription gapped.
    let registry = registry();
    let mut watched = watch_key(&registry, "k").await;
    let mut elsewhere = watch_key(&registry, "other").await;

    let unknown = parse_publish_payload("k", &Value::String("Z".into()));
    assert_eq!(
        unknown,
        ParsedNotification::Reset {
            key: "k".to_owned()
        }
    );
    let _orphaned = registry.dispatch(&unknown, WatcherKind::Exact);

    assert!(matches!(
        drain(&mut watched).as_slice(),
        [CacheWatchEvent::Reset]
    ));
    assert!(drain(&mut elsewhere).is_empty());
}

#[tokio::test]
async fn a_registry_wide_reset_reaches_every_watcher() {
    // The registrations survive it. `Reset` is non-terminal (DESIGN.md §4.3),
    // so the watcher lists stay as they are and the subscription counts do not
    // move.
    let registry = registry();
    let mut key_watch = watch_key(&registry, "k").await;
    let mut prefix_watch = watch_prefix(&registry, "p/").await;

    assert!(registry.broadcast_reset().await);

    assert!(matches!(
        drain(&mut key_watch).as_slice(),
        [CacheWatchEvent::Reset]
    ));
    assert!(matches!(
        drain(&mut prefix_watch).as_slice(),
        [CacheWatchEvent::Reset]
    ));
    assert_eq!(registry.key_subscription_count(), 1);
    assert_eq!(registry.prefix_pattern_count(), 1);
}

#[tokio::test]
async fn a_watch_survives_a_reset_and_keeps_receiving() {
    // The consumer contract, asserted positively: a `Reset` means *re-read*, so
    // the stream stays open and the next event still arrives on it. The SDK
    // documents `recv() -> None` as the backend having dropped the sender
    // *without* a terminal `Closed` (`cluster-sdk/src/cache/watch.rs`), which is
    // exactly what a consumer must never see here.
    let registry = registry();
    let mut watch = watch_key(&registry, "k").await;

    assert!(registry.broadcast_reset().await);
    assert!(matches!(
        drain(&mut watch).as_slice(),
        [CacheWatchEvent::Reset]
    ));

    let orphaned = registry.dispatch(&changed("k"), WatcherKind::Exact);
    assert!(orphaned.is_empty());
    assert_eq!(
        changed_keys(&drain(&mut watch)),
        vec!["k".to_owned()],
        "a reset watcher must still receive the events that follow"
    );

    // And the terminal path still clears what the reset left in place.
    registry.close_all().await;
    assert!(matches!(
        drain(&mut watch).as_slice(),
        [CacheWatchEvent::Closed(ClusterError::Shutdown)]
    ));
    assert_eq!(registry.key_subscription_count(), 0);
}

#[tokio::test]
async fn close_all_delivers_the_terminal_event_to_every_watcher() {
    let registry = registry();
    let mut key_watch = watch_key(&registry, "k").await;
    let mut prefix_watch = watch_prefix(&registry, "p/").await;

    registry.close_all().await;

    for events in [drain(&mut key_watch), drain(&mut prefix_watch)] {
        assert!(
            matches!(
                events.as_slice(),
                [CacheWatchEvent::Closed(ClusterError::Shutdown)]
            ),
            "got {events:?}"
        );
    }
}

#[tokio::test]
async fn no_reset_is_ever_delivered_after_a_terminal_close() {
    // The SDK's `CacheWatch` contract forbids anything following a `Closed`.
    let registry = registry();
    let mut watch = watch_key(&registry, "k").await;

    registry.close_all().await;
    assert!(
        !registry.broadcast_reset().await,
        "a Reset after the registry closed must be suppressed, not delivered"
    );

    assert!(matches!(
        drain(&mut watch).as_slice(),
        [CacheWatchEvent::Closed(ClusterError::Shutdown)]
    ));
}

#[tokio::test]
async fn a_watch_registered_after_close_gets_its_terminal_event_immediately() {
    // Otherwise it registers into a map nothing will ever dispatch to again and
    // silently receives nothing forever.
    let registry = registry();
    registry.close_all().await;

    let mut late = watch_key(&registry, "k").await;

    assert!(matches!(
        drain(&mut late).as_slice(),
        [CacheWatchEvent::Closed(ClusterError::Shutdown)]
    ));
}

#[tokio::test]
async fn a_late_registration_is_told_the_error_that_actually_closed_the_registry() {
    // Not a hardcoded `Shutdown`: only a `ConnectionLost` is
    // `ClusterError::is_retryable`, and the SDK's `RestartingWatch` branches on
    // exactly that. Told `Shutdown`, a consumer's retry policy would never run
    // against what is in fact a recoverable outage.
    let registry = registry();
    registry
        .close_all_with(crate::subscriber::subscriber_lost())
        .await;

    let mut late = watch_key(&registry, "k").await;
    let events = drain(&mut late);
    let [CacheWatchEvent::Closed(err)] = events.as_slice() else {
        panic!("expected one terminal event, got {events:?}");
    };
    assert!(
        err.is_retryable(),
        "a lost subscriber must close retryably, got {err:?}"
    );
}

#[tokio::test]
async fn the_terminal_event_reaches_a_watcher_whose_buffer_is_full() {
    // Delivered with a blocking `send` rather than the fan-out's `try_send`, so
    // a momentarily-full consumer still gets the typed `Closed` instead of a
    // bare channel close it cannot tell apart from a dropped sender.
    let registry = registry();
    let mut watch = watch_key(&registry, "k").await;
    for _ in 0..WATCH_BUFFER {
        let _orphaned = registry.dispatch(&changed("k"), WatcherKind::Exact);
    }

    let close = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { registry.close_all().await }
    });
    // Free a slot so the blocking terminal send can land.
    let _drained = drain(&mut watch);
    close.await.expect("close_all completes");

    let remaining = drain(&mut watch);
    assert!(
        remaining
            .iter()
            .any(|event| matches!(event, CacheWatchEvent::Closed(_))),
        "the typed terminal event must reach a consumer that was briefly full: got {remaining:?}"
    );
}

// ---------------------------------------------------------------------------
// Payload and channel mapping.
// ---------------------------------------------------------------------------

#[test]
fn published_payloads_map_to_their_events() {
    assert_eq!(
        parse_publish_payload("k", &Value::String("C".into())),
        changed("k")
    );
    assert_eq!(
        parse_publish_payload("k", &Value::String("D".into())),
        ParsedNotification::Deleted {
            key: "k".to_owned()
        }
    );
}

#[test]
fn an_eviction_maps_to_deleted_and_not_to_expired() {
    // DESIGN.md §3.7: no TTL lapsed. Telling a consumer the entry aged out when
    // in fact the instance is misconfigured would mislead it about which of the
    // two problems it has.
    assert_eq!(
        parse_keyspace_event("k", "evicted"),
        ParsedNotification::Deleted {
            key: "k".to_owned()
        }
    );
    assert_eq!(
        parse_keyspace_event("k", "expired"),
        ParsedNotification::Expired {
            key: "k".to_owned()
        }
    );
}

#[test]
fn an_unasked_for_keyspace_event_is_dropped_rather_than_reset() {
    // These arrive only when the server's flags are wider than `Kxe`, and there
    // is one per Redis *command* — turning them into resets would make an
    // over-configured server cost a re-read on every mutation.
    for event in ["hset", "del", "expire", "rename_from"] {
        assert_eq!(
            parse_keyspace_event("k", event),
            ParsedNotification::Ignored,
            "`{event}` is already covered by the in-script publish"
        );
    }
}

#[test]
fn channel_names_round_trip_to_the_key() {
    let names = names();
    let channel = names.channel_for_key("tenant-1/a");
    assert_eq!(channel, "cluster:e:c:tenant-1/a");
    assert_eq!(
        names.key_from_event_channel(&channel).as_deref(),
        Some("tenant-1/a")
    );
}

#[test]
fn an_entry_key_round_trips_back_to_the_consumers_key() {
    // `fred` splits a keyspace notification into `(db, key, operation)` before
    // it reaches this plugin, so the recovery here is from the Redis *key*
    // rather than from the channel. The pattern that delivers it is plugin-wide
    // and lives on `KeyspaceNames`, not here — see `subscriber.rs`.
    let names = ChannelNames::new("cluster", 3);
    assert_eq!(names.database(), 3);
    assert_eq!(
        names.key_from_entry_key("cluster:c:tenant-1/a").as_deref(),
        Some("tenant-1/a")
    );
}

#[test]
fn a_foreign_channel_or_key_is_not_this_caches() {
    let names = names();
    assert_eq!(names.key_from_event_channel("someone-else:e:c:k"), None);
    assert_eq!(names.key_from_event_channel("cluster:e:l:some-lock"), None);
    // The lock primitive's own keys share the operator prefix but not the
    // cache's `:c:` segment, so an expiry on a lease must not be reported as a
    // cache deletion.
    assert_eq!(names.key_from_entry_key("cluster:l:some-lock"), None);
    assert_eq!(names.key_from_entry_key("someone-elses:key"), None);
}

#[test]
fn a_prefix_pattern_escapes_glob_metacharacters() {
    // Worse here than in `scan_prefix`: an unescaped `[` would subscribe the
    // watcher to keys it does not watch *and* miss the ones it does.
    let names = names();
    assert_eq!(
        names.pattern_for_prefix("lit[0]/"),
        r"cluster:e:c:lit\[0\]/*"
    );
    assert_eq!(names.pattern_for_prefix("t/"), "cluster:e:c:t/*");
}

#[test]
fn a_prefix_pattern_escapes_the_operators_prefix_too() {
    // The consumer's half was escaped from the start; the `<key_prefix>:e:c:`
    // stem was not, and the operator's `key_prefix` is embedded in it with no
    // validation constraining its charset. Unescaped, `key_prefix: "tenant[a]"`
    // makes the stem a character class: every prefix watch this cache registers
    // would subscribe to `tenanta:e:c:...` and receive nothing that was ever
    // published. `LockNames::release_pattern` and `KeyspaceNames::new` already
    // escape the same value, so this closes the last site that did not.
    let names = ChannelNames::new("tenant[a]", 0);
    assert_eq!(
        names.pattern_for_prefix("lit[0]/"),
        r"tenant\[a\]:e:c:lit\[0\]/*"
    );
}
