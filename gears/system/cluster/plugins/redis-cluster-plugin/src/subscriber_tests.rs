use super::*;

/// The combined plugin's keyspace naming: both primitives under one prefix.
fn combined() -> KeyspaceNames {
    KeyspaceNames::new(
        "cluster",
        3,
        Some(ChannelNames::new("cluster", 3)),
        LockNames::new("cluster"),
    )
}

/// The standalone lock plugin's: the same prefix, no cache half.
fn lock_only() -> KeyspaceNames {
    KeyspaceNames::new("cluster", 0, None, LockNames::new("cluster"))
}

#[test]
fn the_keyspace_pattern_spans_the_whole_prefix_rather_than_the_caches_share_of_it() {
    // The regression this file exists for. A pattern of
    // `__keyspace@3__:cluster:c:*` observes every eviction except a lock
    // lease's — which is the case DESIGN.md §3.7 opens with and the worst one it
    // names, because an evicted lease hands the lock to a second holder with no
    // TTL having lapsed and nothing else in the system able to notice.
    assert_eq!(combined().pattern(), "__keyspace@3__:cluster:*");
    assert_eq!(combined().database(), 3);
    assert!(
        !combined().pattern().contains(":c:"),
        "a pattern narrowed to the cache's segment cannot see an evicted lease"
    );
}

#[test]
fn the_pattern_stays_scoped_to_this_plugins_prefix_and_database() {
    // Both halves of the scoping matter on a shared Redis: the prefix keeps
    // unrelated tenants' keyspace traffic off this subscriber, and the database
    // keeps a notification from another logical db from being read as one of
    // this plugin's own keys.
    let names = KeyspaceNames::new(
        "other-deployment",
        7,
        None,
        LockNames::new("other-deployment"),
    );
    assert_eq!(names.pattern(), "__keyspace@7__:other-deployment:*");
    assert_eq!(names.database(), 7);
}

#[test]
fn the_pattern_escapes_glob_metacharacters_in_the_operators_prefix() {
    // Nothing rules out a `[` in `key_prefix`, and unescaped it is a character
    // class: the subscription would silently cover a different key space than
    // the one this plugin writes to, so evictions of its own keys would arrive
    // for some prefixes and not others.
    let names = KeyspaceNames::new("we[i]rd", 0, None, LockNames::new("we[i]rd"));
    assert_eq!(names.pattern(), "__keyspace@0__:we\\[i\\]rd:*");
}

#[test]
fn a_cache_entry_and_a_lock_lease_are_each_attributed_to_their_own_primitive() {
    let names = combined();
    assert_eq!(
        names.classify("cluster:c:tenant-1/a"),
        Some(OwnedKey::Cache("tenant-1/a".to_owned()))
    );
    assert_eq!(
        names.classify("cluster:l:tenant-1/leader"),
        Some(OwnedKey::Lock("tenant-1/leader".to_owned()))
    );
}

#[test]
fn the_primitive_label_matches_the_family_the_key_came_from() {
    // The label is what makes the eviction counter actionable: an evicted entry
    // costs a re-read, an evicted lease means two holders believe they hold one
    // lock. An alert that cannot separate them has to treat every eviction as
    // one or the other, and both readings are wrong.
    assert_eq!(
        OwnedKey::Cache("a".to_owned()).primitive(),
        Primitive::Cache
    );
    assert_eq!(OwnedKey::Lock("a".to_owned()).primitive(), Primitive::Lock);
    assert_eq!(Primitive::Cache.label(), "cache");
    assert_eq!(Primitive::Lock.label(), "lock");
    assert_eq!(OwnedKey::Cache("a".to_owned()).name(), "a");
    assert_eq!(OwnedKey::Lock("b".to_owned()).name(), "b");
}

#[test]
fn a_key_under_this_prefix_owned_by_neither_primitive_is_declined() {
    let names = combined();
    // The plugin's own *event channels* share the operator prefix and match the
    // pattern. No key exists at a channel name, so nothing real is dropped —
    // but classifying one as an entry would report an eviction of a key that
    // never existed.
    assert_eq!(names.classify("cluster:e:c:tenant-1/a"), None);
    assert_eq!(names.classify("cluster:e:l:tenant-1/leader"), None);
    // And a future key family under this prefix is declined rather than
    // guessed at.
    assert_eq!(names.classify("cluster:q:job-1"), None);
}

#[test]
fn another_deployments_key_is_never_this_plugins() {
    let names = combined();
    assert_eq!(names.classify("someone-else:c:a"), None);
    assert_eq!(names.classify("someone-else:l:a"), None);
    assert_eq!(names.classify("no-prefix-at-all"), None);
}

#[test]
fn the_standalone_lock_plugin_claims_leases_and_disclaims_cache_entries() {
    // Its `cache` half is `None`, so a `<prefix>:c:` key belongs to a *different*
    // deployment sharing the prefix — a plausible arrangement, since the two
    // plugins are independent (DESIGN.md §3.5) — and reporting an eviction of
    // one would attribute another deployment's incident to this one.
    let names = lock_only();
    assert_eq!(
        names.classify("cluster:l:tenant-1/leader"),
        Some(OwnedKey::Lock("tenant-1/leader".to_owned()))
    );
    assert_eq!(names.classify("cluster:c:tenant-1/a"), None);
}

#[test]
fn the_standalone_plugin_still_watches_the_whole_prefix() {
    // It subscribes the same wide pattern rather than a lease-only one. The
    // narrower `<prefix>:l:*` would work today and would silently stop covering
    // a second lock-key family the moment one existed; the classifier is what
    // decides ownership, and it is the same classifier in both plugins.
    assert_eq!(lock_only().pattern(), "__keyspace@0__:cluster:*");
}

// ---------------------------------------------------------------------------
// The reconnect observer's two non-terminal arms (DESIGN.md §4.3, §9).
// ---------------------------------------------------------------------------

/// A registry with no subscriber behind it — every subscription decision
/// short-circuits on `None`, so the broadcast runs exactly as it does in
/// production while nothing reaches a server.
fn watch_registry(signals: &Arc<RedisSignals>) -> Arc<WatchRegistry> {
    WatchRegistry::new(None, Arc::clone(signals))
}

#[tokio::test]
async fn a_missed_reconnect_notification_resets_every_watcher_and_says_so() {
    // Both halves together, deliberately. This arm moved
    // `cluster_watch_resets_total` and
    // `cluster_redis_subscriber_resubscribes_total` and logged nothing, so the
    // counter and the log stream disagreed — an operator saw a reset in the
    // metric with no line anywhere explaining it. DESIGN.md §4.3 states that
    // each `Reset` logs `cluster.watch.reset` *and* increments the counter, so
    // asserting either one alone would leave the defect reachable.
    let (signals, recorder, readback) = crate::test_support::recording_metered_signals();
    let registry = watch_registry(&signals);
    let (_guard, log) = crate::test_support::scoped_capture();

    let flow = observe_reconnect(Err(RecvError::Lagged(7)), &registry, &signals).await;

    assert!(
        flow.is_continue(),
        "a missed notification is still a reconnect, so the observer carries on"
    );
    assert_eq!(
        recorder.watch_resets(),
        vec!["cache".to_owned()],
        "the counter must move - this is the half that always worked"
    );
    assert_eq!(
        readback.counter(crate::observability::SUBSCRIBER_RESUBSCRIBES),
        1,
        "and the Redis-specific counter with it. Asserted because the comment above names both: \
         without this, `signals.subscriber_resubscribed()` could be deleted from the Lagged arm \
         and nothing would fail, leaving one flap and a flap per minute indistinguishable in the \
         metric"
    );
    assert_eq!(
        crate::test_support::count_occurrences(&log, logs::WATCH_RESET),
        1,
        "and the catalogued line must accompany it. Captured: {}",
        crate::test_support::captured(&log)
    );
    assert!(
        crate::test_support::captured(&log).contains("missed=7"),
        "carrying the count of unobserved reconnects, which is the one thing this path can say \
         that the Ok path cannot. Captured: {}",
        crate::test_support::captured(&log)
    );
}

#[tokio::test]
async fn a_closed_notification_stream_ends_the_observer() {
    // The one terminal arm: the sender is gone, so no further notification can
    // arrive. Distinguished from `Lagged` because getting it wrong in either
    // direction is a live task spinning on a dead channel, or an observer that
    // stops resetting after one missed notification.
    let (signals, recorder) = crate::test_support::recording_signals();
    let registry = watch_registry(&signals);

    let flow = observe_reconnect(Err(RecvError::Closed), &registry, &signals).await;

    assert!(flow.is_break());
    assert!(
        recorder.watch_resets().is_empty(),
        "a closed stream is not a reconnect and must reset nothing"
    );
}
