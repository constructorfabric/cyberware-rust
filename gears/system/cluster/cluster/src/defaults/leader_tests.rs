use std::sync::Arc;
use std::time::Duration;

use super::CasBasedLeaderElectionBackend;
use crate::defaults::ShutdownRevoke;
use crate::defaults::test_cache::MemoryCache;
use cluster_sdk::cache::ClusterCacheBackend;
use cluster_sdk::cache::types::{PutRequest, Ttl};
use cluster_sdk::error::ClusterError;
use cluster_sdk::leader::{ElectionConfig, LeaderElectionBackend, LeaderStatus, LeaderWatchEvent};
use cluster_sdk::lease::LeaseToken;

async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn new_rejects_eventually_consistent_cache() {
    let cache = MemoryCache::eventually_consistent();
    assert!(matches!(
        CasBasedLeaderElectionBackend::new(cache),
        Err(ClusterError::InvalidConfig { .. })
    ));
}

#[tokio::test]
async fn new_accepts_linearizable_cache() {
    let cache = MemoryCache::linearizable();
    assert!(CasBasedLeaderElectionBackend::new(cache).is_ok());
}

#[tokio::test]
async fn weak_consistency_constructor_always_succeeds_and_features_track_cache() {
    let weak = CasBasedLeaderElectionBackend::new_allow_weak_consistency(
        MemoryCache::eventually_consistent(),
    );
    assert!(!weak.features().linearizable);
    let strong =
        CasBasedLeaderElectionBackend::new_allow_weak_consistency(MemoryCache::linearizable());
    assert!(strong.features().linearizable);
}

#[tokio::test]
async fn graceful_shutdown_revokes_leader_then_closes_terminally() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(cache) else {
        panic!("linearizable cache must construct");
    };
    let Ok(mut watch) = backend.elect("primary").await else {
        panic!("election must join");
    };
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Leader)
    ));
    assert!(watch.is_leader());

    // Graceful cluster shutdown. `revoke` awaits the election task's revocation
    // emit, so the leader has observed loss by the time it returns.
    backend.revoke().await;

    // Loss is observed before the terminal close (cpt-cf-clst-fr-shutdown-revoke),
    // and the synchronous snapshot no longer reports leadership.
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Lost)
    ));
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Closed(ClusterError::Shutdown)
    ));
    assert_eq!(watch.status(), LeaderStatus::Lost);
    assert!(!watch.is_leader());
}

#[tokio::test]
async fn single_candidate_becomes_leader() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(cache) else {
        panic!("linearizable cache must construct");
    };
    let Ok(mut watch) = backend.elect("primary").await else {
        panic!("election must join");
    };
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Leader)
    ));
    assert!(watch.is_leader());
}

#[tokio::test]
async fn elects_leader_over_a_watchless_cache() {
    // A cache that serves no exact watch (features().watch == false, watch()
    // -> Unsupported) must not fail elect(): the CAS election reconciles off the
    // renewal timer alone rather than propagating the Unsupported error from the
    // pre-claim subscribe. Regression for the live redis `watch_mode: disabled`
    // bug (plan D3).
    let cache = MemoryCache::linearizable_without_watch();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(cache) else {
        panic!("linearizable cache must construct");
    };
    let Ok(mut watch) = backend.elect("primary").await else {
        panic!("election must join even without a cache watch");
    };
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Leader)
    ));
    assert!(watch.is_leader());
}

#[tokio::test]
async fn second_candidate_is_follower() {
    let cache = MemoryCache::linearizable();
    let Ok(a) = CasBasedLeaderElectionBackend::new(Arc::clone(&cache) as _) else {
        panic!("construct a");
    };
    let Ok(b) = CasBasedLeaderElectionBackend::new(cache as _) else {
        panic!("construct b");
    };
    let Ok(mut watch_a) = a.elect("primary").await else {
        panic!("a joins");
    };
    assert!(matches!(
        watch_a.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Leader)
    ));
    let Ok(mut watch_b) = b.elect("primary").await else {
        panic!("b joins");
    };
    assert!(matches!(
        watch_b.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Follower)
    ));
    assert!(!watch_b.is_leader());
}

#[tokio::test]
async fn foreign_takeover_emits_lost_then_resolves() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(Arc::clone(&cache) as _) else {
        panic!("construct");
    };
    let Ok(mut watch) = backend.elect("primary").await else {
        panic!("join");
    };
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Leader)
    ));
    // A foreign holder overwrites the claim — split-brain takeover.
    assert!(
        cache
            .put(PutRequest {
                key: "election/primary",
                value: b"intruder",
                ttl: Ttl::Of(Duration::from_secs(30)),
            })
            .await
            .is_ok()
    );
    // The watch observes the loss, then resolves to follower.
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Lost)
    ));
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Follower)
    ));
    // The intruder leaves; the watch auto-reenrolls back to leader. Bounded so a
    // re-election regression fails the test fast instead of hanging CI.
    assert!(cache.delete("election/primary").await.is_ok());
    let reenroll = async {
        loop {
            match watch.changed().await {
                LeaderWatchEvent::Status(LeaderStatus::Leader) => break,
                LeaderWatchEvent::Status(_) | LeaderWatchEvent::Reset => {}
                other => panic!("unexpected event while reenrolling: {other:?}"),
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(5), reenroll)
        .await
        .is_err()
    {
        panic!("timed out waiting for re-enrollment to leader");
    }
}

#[tokio::test]
async fn resign_releases_the_claim() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(Arc::clone(&cache) as _) else {
        panic!("construct");
    };
    let Ok(mut watch) = backend.elect("primary").await else {
        panic!("join");
    };
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Leader)
    ));
    assert!(watch.resign().await.is_ok());
    settle().await;
    let Ok(after) = cache.get("election/primary").await else {
        panic!("get must succeed");
    };
    assert!(after.is_none(), "resign must release the claim");
}

#[tokio::test]
async fn dropping_watch_releases_claim_best_effort() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(Arc::clone(&cache) as _) else {
        panic!("construct");
    };
    let Ok(mut watch) = backend.elect("primary").await else {
        panic!("join");
    };
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Leader)
    ));
    drop(watch);
    settle().await;
    let Ok(after) = cache.get("election/primary").await else {
        panic!("get must succeed");
    };
    assert!(
        after.is_none(),
        "dropping the watch best-effort releases the claim"
    );
}

#[tokio::test]
async fn compare_and_delete_is_guarded_by_value_not_version() {
    // The primitive the guarded release (`release_if_holder`) relies on, exercised
    // against the spurious-flap race: an owner re-claims a key after a successor
    // already took it over, so both claims sit at version 1 (a fresh
    // `put_if_absent` resets the version). A value guard distinguishes them where
    // a version guard would alias and wipe the successor's claim.
    let cache = MemoryCache::linearizable();
    // Owner A claims (fresh entry, version 1).
    assert!(matches!(
        cache
            .put_if_absent(PutRequest {
                key: "k",
                value: b"owner-a",
                ttl: Ttl::Indefinite,
            })
            .await,
        Ok(Some(_))
    ));
    // A's claim lapses and successor B re-claims — also a fresh entry at version 1.
    assert!(cache.delete("k").await.is_ok());
    assert!(matches!(
        cache
            .put_if_absent(PutRequest {
                key: "k",
                value: b"owner-b",
                ttl: Ttl::Indefinite,
            })
            .await,
        Ok(Some(_))
    ));

    // A's late release must NOT wipe B's claim: the value no longer matches.
    let Ok(deleted) = cache.compare_and_delete("k", b"owner-a").await else {
        panic!("compare_and_delete must succeed");
    };
    assert!(
        !deleted,
        "a value mismatch must not delete the successor's claim"
    );
    let Ok(Some(entry)) = cache.get("k").await else {
        panic!("the successor's claim must survive");
    };
    assert_eq!(entry.value, b"owner-b".to_vec());

    // B releasing its own claim deletes it.
    let Ok(deleted) = cache.compare_and_delete("k", b"owner-b").await else {
        panic!("compare_and_delete must succeed");
    };
    assert!(deleted, "the matching owner must delete its own claim");
    assert!(matches!(cache.get("k").await, Ok(None)));
}

#[tokio::test(start_paused = true)]
async fn renewal_extends_the_lease_beyond_the_initial_ttl() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(Arc::clone(&cache) as _)
        .map(CasBasedLeaderElectionBackend::with_virtual_clock)
    else {
        panic!("construct");
    };
    // Default config: ttl 30s, renewal interval 10s.
    let Ok(mut watch) = backend.elect("primary").await else {
        panic!("join");
    };
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Leader)
    ));
    // Advance well past the initial 30s TTL in renewal-sized steps; the
    // renewal CAS must keep extending the lease so leadership never lapses.
    for _ in 0..6 {
        tokio::time::advance(Duration::from_secs(11)).await;
        settle().await;
    }
    assert!(
        watch.is_leader(),
        "renewal must preserve leadership past the initial TTL"
    );
    let Ok(entry) = cache.get("election/primary").await else {
        panic!("get must succeed");
    };
    assert!(entry.is_some(), "the renewed claim must still be present");
}

// The lease-token half of the trait (§5.8.1, item `L1`)

/// Two backends over one cache — the in-process stand-in for two cluster replicas
/// over one backing store.
fn two_handles(
    cache: &Arc<MemoryCache>,
) -> (CasBasedLeaderElectionBackend, CasBasedLeaderElectionBackend) {
    // Virtual clock on both handles: these two-replica tests lapse claims under
    // `tokio::time::advance`, which the production wall clock (H3) never moves.
    let build = || {
        CasBasedLeaderElectionBackend::new(Arc::clone(cache) as Arc<dyn ClusterCacheBackend>)
            .expect("a linearizable cache must construct")
            .with_virtual_clock()
    };
    (build(), build())
}

#[tokio::test]
async fn join_takes_the_claim_once_and_then_reports_a_follower() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(cache) else {
        panic!("construct");
    };
    let config = ElectionConfig::default();
    let Ok(Some(token)) = backend.join("primary", "cand-a", config).await else {
        panic!("the first candidate must take the claim");
    };
    assert_eq!(token.name, "primary");
    assert_eq!(token.owner, "cand-a");
    assert_eq!(token.fence, 1);
    assert!(
        matches!(backend.join("primary", "cand-b", config).await, Ok(None)),
        "losing an election is an ordinary outcome, not an error"
    );
}

#[tokio::test]
async fn a_claim_is_renewable_and_resignable_through_another_backend_handle() {
    // The property that lets a leader survive the replica it was elected through
    // (invariant I7).
    let cache = MemoryCache::linearizable();
    let (elector, other_replica) = two_handles(&cache);
    let config = ElectionConfig::default();

    let Ok(Some(token)) = elector.join("primary", "cand-a", config).await else {
        panic!("join");
    };
    assert!(
        other_replica.renew(&token, config.ttl()).await.is_ok(),
        "a replica that never saw the join must serve the renew"
    );
    assert!(other_replica.resign(&token).await.is_ok());
    // The election is open again.
    assert!(matches!(
        elector.join("primary", "cand-b", config).await,
        Ok(Some(_))
    ));
}

#[tokio::test]
async fn renewing_a_claim_that_is_not_yours_is_expired() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(cache) else {
        panic!("construct");
    };
    let config = ElectionConfig::default();
    let Ok(Some(token)) = backend.join("primary", "cand-a", config).await else {
        panic!("join");
    };
    let impostor = LeaseToken::new(&token.name, "cand-b", token.fence);
    assert!(matches!(
        backend.renew(&impostor, config.ttl()).await,
        Err(ClusterError::LockExpired { name }) if name == "primary"
    ));
}

#[tokio::test]
async fn resigning_a_claim_nobody_holds_is_ok() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(cache) else {
        panic!("construct");
    };
    assert!(
        backend
            .resign(&LeaseToken::new("primary", "cand-a", 1))
            .await
            .is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn taking_a_lapsed_claim_strictly_increases_the_fence() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(cache)
        .map(CasBasedLeaderElectionBackend::with_virtual_clock)
    else {
        panic!("construct");
    };
    let config = ElectionConfig::new(Duration::from_secs(5), 3).expect("a valid election config");
    let Ok(Some(first)) = backend.join("primary", "cand-a", config).await else {
        panic!("join");
    };
    tokio::time::advance(Duration::from_secs(6)).await;
    let Ok(Some(second)) = backend.join("primary", "cand-b", config).await else {
        panic!("a lapsed claim must be takeable");
    };
    assert!(
        second.fence > first.fence,
        "{} !> {}",
        second.fence,
        first.fence
    );
    // And the fenced-out predecessor cannot renew its way back in.
    assert!(matches!(
        backend.renew(&first, config.ttl()).await,
        Err(ClusterError::LockExpired { .. })
    ));
}

#[tokio::test]
async fn an_elected_leader_and_a_token_claim_are_the_same_lease() {
    // `elect` and `join` compete for one record, so a leader elected through the
    // watch path blocks a token-path candidate.
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(cache) else {
        panic!("construct");
    };
    let Ok(mut watch) = backend.elect("primary").await else {
        panic!("elect");
    };
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Leader)
    ));
    assert!(matches!(
        backend
            .join("primary", "cand-b", ElectionConfig::default())
            .await,
        Ok(None)
    ));
}

// B7 — the state machine must never await a consumer send

/// The consumer-visible event buffer, mirrored from the backend so a change to
/// one makes these tests fail loudly rather than quietly stop overrunning it.
const EVENT_BUFFER: usize = super::EVENT_BUFFER;

/// Comfortably more `Reset`s than the event buffer holds — the reviewer's
/// reproduction used 40, which is what a couple of dozen LISTEN reconnects on a
/// flapping connection looks like.
const RESET_BURST: usize = 40;

/// An election config with a TTL short enough to lapse inside a test.
fn short_lived() -> ElectionConfig {
    ElectionConfig::new(Duration::from_secs(5), 3).expect("a valid election config")
}

/// **The reproduction.** A gate-pattern consumer — one that reads `status()` and
/// never drains `changed()`, which `LeaderWatch` documents and permits — must not
/// be able to wedge the renewal loop.
///
/// Before the fix, every consumer-facing send in the state machine was an
/// `.await` inside the `select!`. Once the 16-slot buffer filled, the task parked
/// in `send` forever: it stopped renewing, never re-claimed, and could not even
/// observe its own shutdown token, while the latched snapshot kept answering
/// `Leader`. A rival then took the lapsed claim and there were two leaders at
/// once — which is the single thing this primitive exists to prevent.
#[tokio::test(start_paused = true)]
async fn a_consumer_that_never_drains_cannot_wedge_the_renewal_loop() {
    let cache = MemoryCache::linearizable();
    let (incumbent, rival) = two_handles(&cache);
    let config = short_lived();

    let Ok(watch) = incumbent.elect_with_config("primary", config).await else {
        panic!("the sole candidate enrols");
    };
    settle().await;
    assert!(
        watch.is_leader(),
        "the sole candidate leads; the rest of this test is about whether it keeps the claim"
    );

    // The gate pattern: `watch` is held and never read. The burst is what fills
    // the buffer behind it.
    cache.emit_resets(RESET_BURST).await;
    settle().await;

    // Past the TTL, in renewal-sized steps. A live state machine renews on every
    // one of these; a wedged one renews on none, and the claim lapses.
    for _ in 0..6 {
        tokio::time::advance(config.ttl() / 2).await;
        settle().await;
    }

    // The rival's `join` is the question stated as a consequence: can anyone else
    // take this claim while the incumbent still believes it holds it?
    let taken = rival
        .join("primary", "rival", config)
        .await
        .expect("a join is either a token or a loss, never an error here");
    assert!(
        !(taken.is_some() && watch.is_leader()),
        "two leaders at once: the rival took the claim while the incumbent's snapshot still \
         reports Leader. The incumbent must either keep renewing or stop reporting Leader"
    );
    // And the direction it actually resolves in, so the assertion above cannot be
    // satisfied by an incumbent that simply gave up: a state machine that never
    // blocks keeps its claim through the burst.
    assert!(
        taken.is_none() && watch.is_leader(),
        "the incumbent should have renewed straight through the burst (rival token: \
         {taken:?}, incumbent leads: {})",
        watch.is_leader()
    );
}

/// `revoke()` must complete with a full event buffer.
///
/// The other half of the same wedge: `revoke` cancels the shared token and then
/// **awaits** the election tasks, so a task parked in an awaited consumer send
/// never reaches its `shutdown.cancelled()` arm and `revoke` never returns. That
/// is a hung graceful shutdown, from one consumer that stopped reading.
#[tokio::test]
async fn revoke_completes_with_a_full_event_buffer() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(Arc::clone(&cache) as _) else {
        panic!("construct");
    };
    let Ok(mut watch) = backend.elect_with_config("primary", short_lived()).await else {
        panic!("enrols");
    };
    settle().await;
    cache.emit_resets(RESET_BURST).await;
    settle().await;

    let revoked = tokio::time::timeout(Duration::from_secs(5), backend.revoke()).await;
    assert!(
        revoked.is_ok(),
        "revoke must not depend on the consumer's read cadence; it is still awaiting a wedged \
         election task 5 s later"
    );

    // And the revocation is *delivered*, not merely survived: the two terminal
    // events ride the reserved headroom, so a full buffer cannot cost them.
    let mut terminal = Vec::new();
    while let Ok(event) = tokio::time::timeout(Duration::from_secs(5), watch.changed()).await {
        match event {
            LeaderWatchEvent::Status(status) => terminal.push(format!("Status({status:?})")),
            LeaderWatchEvent::Closed(err) => {
                terminal.push(format!("Closed({err:?})"));
                break;
            }
            _ => {}
        }
    }
    assert!(
        terminal.ends_with(&[
            "Status(Lost)".to_owned(),
            format!("Closed({:?})", ClusterError::Shutdown),
        ]),
        "the shutdown two-step must still arrive behind the backlog, in order (saw {terminal:?})"
    );
}

/// Dropped events are **announced**, not silent: a consumer that fell behind
/// receives a `Lagged` accounting for what it missed before it sees the next
/// event.
///
/// This is the cost the fix trades for: a consumer that previously could not miss
/// a `Reset` now can. That is already the documented cache-watch contract, and
/// announcing the gap is what keeps it a contract rather than a data loss.
#[tokio::test]
async fn a_slow_consumer_is_told_how_many_events_it_missed() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(Arc::clone(&cache) as _) else {
        panic!("construct");
    };
    let Ok(mut watch) = backend.elect_with_config("primary", short_lived()).await else {
        panic!("enrols");
    };
    settle().await;

    // Overrun the buffer while nothing reads.
    cache.emit_resets(RESET_BURST).await;
    settle().await;

    // Drain exactly what the buffer held: the initial `Status(Leader)` plus the
    // `Reset`s that fitted behind it. Everything after that was dropped.
    for index in 0..EVENT_BUFFER {
        let Ok(event) = tokio::time::timeout(Duration::from_secs(5), watch.changed()).await else {
            panic!("event {index} of the full buffer never arrived");
        };
        assert!(
            matches!(
                event,
                LeaderWatchEvent::Status(LeaderStatus::Leader) | LeaderWatchEvent::Reset
            ),
            "the buffer should hold the initial status and then Resets, got {event:?} at {index}"
        );
    }

    // One more event now that there is room: the outstanding lag notice is paid
    // first, which is what "told before you see the next one" means.
    cache.emit_resets(1).await;
    settle().await;
    let Ok(next) = tokio::time::timeout(Duration::from_secs(5), watch.changed()).await else {
        panic!("no further event arrived");
    };
    let LeaderWatchEvent::Lagged { dropped } = next else {
        panic!("the first event after a drained backlog must be the owed Lagged, got {next:?}");
    };
    // The exact count is a property of how many of the burst the fixture's own
    // watch channel accepted, so it is not asserted; that any drop is *reported*
    // is the contract.
    assert!(dropped > 0, "a Lagged must account for at least one drop");
}

/// M13: a consumer that fell behind is told **even when the election then goes
/// quiet** — the owed `Lagged` is flushed from the renewal tick, not left to
/// wait on an unrelated later event.
///
/// The sibling `a_slow_consumer_is_told_how_many_events_it_missed` has to call
/// `emit_resets(1)` to make the notice appear, because `offer` pays the debt
/// only on the *next* event. A stable leader's renewal emits none, so without a
/// proactive flush a `changed()`-only consumer would drain a stale `Leader`
/// backlog and never learn it dropped anything — the silent staleness ADR-003
/// exists to eliminate. Here nothing further is emitted: the tick flush is the
/// only thing that can pay the debt, and the only event delivered after the
/// backlog must be the owed `Lagged`.
#[tokio::test(start_paused = true)]
async fn a_quiescent_election_pays_the_owed_lagged_from_the_renewal_tick() {
    let cache = MemoryCache::linearizable();
    let config = short_lived();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(Arc::clone(&cache) as _) else {
        panic!("construct");
    };
    let Ok(mut watch) = backend.elect_with_config("primary", config).await else {
        panic!("enrols");
    };
    settle().await;
    assert!(watch.is_leader(), "the sole candidate leads");

    // Overrun the buffer while nothing reads, so a Lagged is owed.
    cache.emit_resets(RESET_BURST).await;
    settle().await;

    // Drain exactly what the buffer held — the initial `Status(Leader)` and the
    // `Reset`s behind it. Everything after that was dropped and is now owed.
    for index in 0..EVENT_BUFFER {
        let event = watch.changed().await;
        assert!(
            matches!(
                event,
                LeaderWatchEvent::Status(LeaderStatus::Leader) | LeaderWatchEvent::Reset
            ),
            "the buffer should hold the initial status then Resets, got {event:?} at {index}"
        );
    }

    // The election now goes quiet: no cache event, no status change. The sole
    // candidate simply keeps renewing. Fire one renewal tick — its flush is the
    // only thing that can pay the debt now.
    tokio::time::advance(config.renewal_interval()).await;
    settle().await;

    let next = watch.changed().await;
    let LeaderWatchEvent::Lagged { dropped } = next else {
        panic!(
            "a quiescent election must still pay the owed Lagged from the renewal tick, got \
             {next:?} (no unrelated later event was emitted to carry it)"
        );
    };
    assert!(
        dropped > 0,
        "the tick flush must account for the dropped events"
    );
}

/// A terminal `Closed(err)` must reach the consumer **with its own error**, even
/// when the buffer is full.
///
/// The one send in the state machine for which "drop it and owe a `Lagged`" is
/// the wrong answer: `changed()` synthesizes `Closed(Shutdown)` when the sender
/// is dropped without a terminal event, so a dropped `Closed(Provider{..})`
/// would report a provider failure to the consumer as an orderly shutdown. It
/// rides the reserved terminal headroom instead.
#[tokio::test]
async fn a_terminal_close_keeps_its_error_through_a_full_buffer() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(Arc::clone(&cache) as _) else {
        panic!("construct");
    };
    let Ok(mut watch) = backend.elect_with_config("primary", short_lived()).await else {
        panic!("enrols");
    };
    settle().await;
    cache.emit_resets(RESET_BURST).await;
    settle().await;

    // Non-retryable on purpose: a retryable close is the watch-restart path and
    // belongs to a different finding, while this is about the error surviving.
    let fatal = ClusterError::InvalidConfig {
        reason: "the fixture's backend gave up on the subscription".to_owned(),
    };
    cache.emit_watch_close(&fatal);
    settle().await;

    let mut last = None;
    while let Ok(event) = tokio::time::timeout(Duration::from_secs(5), watch.changed()).await {
        if let LeaderWatchEvent::Closed(err) = event {
            last = Some(err);
            break;
        }
    }
    assert!(
        matches!(last, Some(ClusterError::InvalidConfig { .. })),
        "the backend's own terminal error must reach the consumer, not `changed()`'s \
         synthesized Closed(Shutdown): got {last:?}"
    );
}

// B5 — a *retryable* cache-watch close must not release a live claim

/// A retryable subscription close (the Postgres LISTEN reconnect budget
/// exhausting on an ordinary failover) is a subscription event, not a leadership
/// one: the task must re-`watch` and keep the claim renewing, never release it.
///
/// Before the fix, the `Closed` arm returned `false` regardless of retryability,
/// and the loop's single exit does `release_if_holder()` — so an ordinary
/// failover dropped a live claim. That is the Profile 1 / Profile 3 divergence
/// invariant I1 forbids (Profile 3's remote pump already re-attaches on a
/// retryable close).
#[tokio::test(start_paused = true)]
async fn a_retryable_watch_close_keeps_the_live_claim_profile1() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(Arc::clone(&cache) as _)
        .map(CasBasedLeaderElectionBackend::with_virtual_clock)
    else {
        panic!("construct");
    };
    let config = short_lived();
    let Ok(watch) = backend.elect_with_config("primary", config).await else {
        panic!("enrols");
    };
    settle().await;
    assert!(watch.is_leader(), "the sole candidate leads");

    // A retryable close at the cache watch — exactly what the Postgres cache
    // broadcasts when its LISTEN reconnect budget is exhausted.
    cache.emit_watch_close(&ClusterError::Provider {
        kind: cluster_sdk::error::ProviderErrorKind::ConnectionLost,
        message: "listen reconnect budget exhausted".to_owned(),
    });
    settle().await;

    // The claim survives, and the pump keeps renewing across several intervals:
    // the re-subscribe keeps the feed, the timer keeps the claim.
    for _ in 0..6 {
        tokio::time::advance(config.ttl() / 2).await;
        settle().await;
    }
    assert!(
        watch.is_leader(),
        "a retryable subscription close must not cost the claim (B5, ADR-003, section 6.6)"
    );
    let Ok(record) = cache.get("election/primary").await else {
        panic!("get");
    };
    assert!(
        record.is_some(),
        "the live claim's record must still be present after a retryable close"
    );
}

/// The complementary case: a **non**-retryable close is terminal and still ends
/// the loop, so the fix does not make a fatal error survivable. This is also the
/// mutation guard against "collapse to always-retry".
#[tokio::test]
async fn a_non_retryable_watch_close_still_ends_the_loop_profile1() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(Arc::clone(&cache) as _) else {
        panic!("construct");
    };
    let Ok(mut watch) = backend.elect_with_config("primary", short_lived()).await else {
        panic!("enrols");
    };
    settle().await;
    assert!(watch.is_leader());

    // Non-retryable: the backend gave up on the subscription in a way no
    // re-subscribe can recover.
    let fatal = ClusterError::InvalidConfig {
        reason: "the backend gave up on the subscription".to_owned(),
    };
    cache.emit_watch_close(&fatal);

    let mut last = None;
    while let Ok(event) = tokio::time::timeout(Duration::from_secs(5), watch.changed()).await {
        if let LeaderWatchEvent::Closed(err) = event {
            last = Some(err);
            break;
        }
    }
    assert!(
        matches!(last, Some(ClusterError::InvalidConfig { .. })),
        "a non-retryable close must end the loop terminally with its own error, got {last:?}"
    );
    assert!(
        !watch.is_leader(),
        "and leadership is gone once the loop has torn down"
    );
}

/// `revoke_for_shutdown` still terminates the loop after the B5 change — the
/// re-subscribe path must not swallow the shutdown signal.
#[tokio::test]
async fn revoke_still_terminates_after_a_retryable_close_profile1() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedLeaderElectionBackend::new(Arc::clone(&cache) as _) else {
        panic!("construct");
    };
    let Ok(mut watch) = backend.elect_with_config("primary", short_lived()).await else {
        panic!("enrols");
    };
    // Drain the initial status so the terminal two-step is what we read below.
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Leader)
    ));
    // Put the task on the re-subscribe path first, so we prove revoke wins even
    // there.
    cache.emit_watch_close(&ClusterError::Provider {
        kind: cluster_sdk::error::ProviderErrorKind::ConnectionLost,
        message: "flap".to_owned(),
    });
    settle().await;
    assert!(watch.is_leader(), "the claim survives the retryable close");

    let revoked = tokio::time::timeout(Duration::from_secs(5), backend.revoke()).await;
    assert!(
        revoked.is_ok(),
        "revoke must still terminate the loop after a retryable re-subscribe"
    );
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Lost)
    ));
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Closed(ClusterError::Shutdown)
    ));
    assert!(!watch.is_leader());
}
