use std::sync::Arc;
use std::time::Duration;

use super::CasBasedDistributedLockBackend;
use crate::defaults::ShutdownRevoke;
use crate::defaults::test_cache::MemoryCache;
use cluster_sdk::cache::ClusterCacheBackend;
use cluster_sdk::cache::types::{PutRequest, Ttl};
use cluster_sdk::error::{ClusterError, ProviderErrorKind};
use cluster_sdk::lease::LeaseToken;
use cluster_sdk::lock::DistributedLockBackend;

async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn new_rejects_eventually_consistent_cache() {
    assert!(matches!(
        CasBasedDistributedLockBackend::new(MemoryCache::eventually_consistent()),
        Err(ClusterError::InvalidConfig { .. })
    ));
}

#[tokio::test]
async fn weak_consistency_constructor_succeeds_and_features_track_cache() {
    let weak = CasBasedDistributedLockBackend::new_allow_weak_consistency(
        MemoryCache::eventually_consistent(),
    );
    assert!(!weak.features().linearizable);
    let strong =
        CasBasedDistributedLockBackend::new_allow_weak_consistency(MemoryCache::linearizable());
    assert!(strong.features().linearizable);
}

#[tokio::test]
async fn try_lock_acquires_then_contends_while_held() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("construct");
    };
    let Ok(guard) = backend.try_lock("ledger", Duration::from_secs(30)).await else {
        panic!("free lock must acquire");
    };
    assert_eq!(guard.name(), "ledger");
    // A second attempt while held contends.
    assert!(matches!(
        backend.try_lock("ledger", Duration::from_secs(30)).await,
        Err(ClusterError::LockContended { name }) if name == "ledger"
    ));
}

#[tokio::test]
async fn release_frees_the_lock_for_a_new_acquirer() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("construct");
    };
    let Ok(guard) = backend.try_lock("ledger", Duration::from_secs(30)).await else {
        panic!("acquire");
    };
    assert!(guard.release().await.is_ok());
    settle().await;
    // The next acquirer succeeds.
    let Ok(next) = backend.try_lock("ledger", Duration::from_secs(30)).await else {
        panic!("released lock must be re-acquirable");
    };
    assert_eq!(next.name(), "ledger");
}

#[tokio::test]
async fn release_does_not_delete_a_foreign_holders_entry() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedDistributedLockBackend::new(Arc::clone(&cache) as _) else {
        panic!("construct");
    };
    let Ok(guard) = backend.try_lock("ledger", Duration::from_secs(30)).await else {
        panic!("acquire");
    };
    // A foreign holder takes over the key (e.g. after a TTL lapse).
    assert!(
        cache
            .put(PutRequest {
                key: "lock/ledger",
                value: b"foreign",
                ttl: Ttl::Of(Duration::from_secs(30)),
            })
            .await
            .is_ok()
    );
    // This holder's release must NOT delete the foreign entry.
    assert!(guard.release().await.is_ok());
    settle().await;
    let Ok(Some(entry)) = cache.get("lock/ledger").await else {
        panic!("the foreign entry must remain");
    };
    assert_eq!(entry.value, b"foreign");
}

#[tokio::test(start_paused = true)]
async fn crashed_holder_is_reaped_by_ttl() {
    let cache = MemoryCache::linearizable();
    // Virtual clock: the steal-on-expiry below turns on the lease deadline lapsing
    // under `advance` (H3; the record's physical TTL includes the 1h retention, so
    // it is not the cache sweeper that frees it).
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache)
        .map(CasBasedDistributedLockBackend::with_virtual_clock)
    else {
        panic!("construct");
    };
    let Ok(guard) = backend.try_lock("ledger", Duration::from_secs(5)).await else {
        panic!("acquire");
    };
    // The holder "crashes" — dropping the guard does no I/O.
    drop(guard);
    // Within the TTL the lock is still held.
    assert!(matches!(
        backend.try_lock("ledger", Duration::from_secs(5)).await,
        Err(ClusterError::LockContended { .. })
    ));
    // Past the TTL the entry is reaped and a new acquirer succeeds.
    tokio::time::advance(Duration::from_secs(6)).await;
    settle().await;
    let Ok(reacquired) = backend.try_lock("ledger", Duration::from_secs(5)).await else {
        panic!("TTL-reaped lock must be re-acquirable");
    };
    assert_eq!(reacquired.name(), "ledger");
}

#[tokio::test]
async fn blocking_lock_waits_then_acquires_on_release() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("construct");
    };
    let backend = Arc::new(backend);
    let Ok(guard) = backend.try_lock("ledger", Duration::from_secs(30)).await else {
        panic!("first holder acquires");
    };
    // A waiter blocks for the lock.
    let waiter_backend = Arc::clone(&backend);
    let waiter = tokio::spawn(async move {
        waiter_backend
            .lock("ledger", Duration::from_secs(30), Duration::from_secs(30))
            .await
    });
    // Give the waiter time to block on the watch, then release.
    settle().await;
    assert!(guard.release().await.is_ok());
    let Ok(joined) = waiter.await else {
        panic!("waiter task must join");
    };
    let Ok(acquired) = joined else {
        panic!("waiter must acquire after release");
    };
    assert_eq!(acquired.name(), "ledger");
}

#[tokio::test]
async fn blocking_lock_acquires_uncontended_over_a_watchless_cache() {
    // A cache that serves no exact watch (features().watch == false, watch()
    // -> Unsupported) must not fail a blocking lock(). The pre-fix code
    // subscribed *before* the first claim, so even an uncontended acquisition
    // failed at that `?`. Now the wait degrades to bounded polling. Regression
    // for the live redis `watch_mode: disabled` bug (plan D3).
    let cache = MemoryCache::linearizable_without_watch();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("linearizable cache must construct");
    };
    let Ok(guard) = backend
        .lock("ledger", Duration::from_secs(30), Duration::from_secs(30))
        .await
    else {
        panic!("uncontended blocking lock must acquire without a cache watch");
    };
    assert_eq!(guard.name(), "ledger");
}

#[tokio::test]
async fn blocking_lock_polls_to_acquire_over_a_watchless_cache() {
    // Contended: with no watch to wake on, the waiter must poll the claim on the
    // bounded interval and acquire once the holder releases.
    let cache = MemoryCache::linearizable_without_watch();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("linearizable cache must construct");
    };
    let backend = Arc::new(backend);
    let Ok(guard) = backend.try_lock("ledger", Duration::from_secs(30)).await else {
        panic!("first holder acquires");
    };
    let waiter_backend = Arc::clone(&backend);
    let waiter = tokio::spawn(async move {
        waiter_backend
            .lock("ledger", Duration::from_secs(30), Duration::from_secs(30))
            .await
    });
    // Let the waiter block (its first claim contends), then release; the waiter
    // acquires on its next poll rather than on a watch event.
    settle().await;
    assert!(guard.release().await.is_ok());
    let Ok(joined) = waiter.await else {
        panic!("waiter task must join");
    };
    let Ok(acquired) = joined else {
        panic!("waiter must acquire after release via polling");
    };
    assert_eq!(acquired.name(), "ledger");
}

#[tokio::test(start_paused = true)]
async fn blocking_lock_on_unusable_watch_fails_fast_without_spinning() {
    // A backend whose `watch` ends immediately on every subscribe would make
    // the blocking loop busy-spin (claim → watch ends → re-subscribe …) until
    // the timeout. The re-subscribe cap must surface a provider error instead.
    let cache = MemoryCache::linearizable_with_dead_watch();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("construct");
    };
    let backend = Arc::new(backend);
    // Hold the lock with a long TTL so the waiter can never claim it and the
    // entry is never reaped during the test.
    let Ok(_held) = backend.try_lock("ledger", Duration::from_secs(100)).await else {
        panic!("hold the lock");
    };
    // The dead watch resolves instantly, so each backoff sleep auto-advances
    // paused time by a few tens of ms; the cap is hit well before the 90s
    // acquisition timeout, proving it fails fast rather than spinning until
    // LockTimeout. A structurally unusable watch is non-retryable (`Other`).
    let result = backend
        .lock("ledger", Duration::from_secs(100), Duration::from_secs(90))
        .await;
    assert!(matches!(
        result,
        Err(ClusterError::Provider {
            kind: ProviderErrorKind::Other,
            ..
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn blocking_lock_tight_timeout_does_not_overshoot_a_full_backoff() {
    // With a timeout shorter than one backoff interval, an immediately-ending
    // watch must not be slept past the caller's deadline: the wait bounds out
    // as LockTimeout (the cap is never reached) within ~the timeout, not after
    // a full 50ms backoff.
    let cache = MemoryCache::linearizable_with_dead_watch();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("construct");
    };
    let backend = Arc::new(backend);
    let Ok(_held) = backend.try_lock("ledger", Duration::from_secs(100)).await else {
        panic!("hold the lock");
    };
    let timeout = Duration::from_millis(10);
    let start = tokio::time::Instant::now();
    let result = backend
        .lock("ledger", Duration::from_secs(100), timeout)
        .await;
    let waited = start.elapsed();
    assert!(
        matches!(result, Err(ClusterError::LockTimeout { ref name, .. }) if name == "ledger"),
        "tight timeout against a dead watch must time out, not hit the cap; got {result:?}"
    );
    assert!(
        waited < Duration::from_millis(40),
        "backoff must be clamped to the remaining timeout (no full-backoff overshoot); \
             waited {waited:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn blocking_lock_keeps_waiting_across_legitimate_watch_rotations() {
    // A backend whose `watch` ends with `None` only after living a meaningful
    // interval is performing legitimate stream rotation, not busy-spinning.
    // Many such rotations (well past the cap) must NOT abort the wait as a
    // provider error — the acquisition stays bounded by its own timeout.
    let cache = MemoryCache::linearizable_with_rotating_watch();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("construct");
    };
    let backend = Arc::new(backend);
    // Hold the lock with a long TTL so the waiter never claims it.
    let Ok(_held) = backend.try_lock("ledger", Duration::from_secs(1000)).await else {
        panic!("hold the lock");
    };
    let waiter_backend = Arc::clone(&backend);
    let waiter = tokio::spawn(async move {
        // The watch rotates every 200ms; across a 5s timeout that is ~25
        // rotations (>> the cap of 8), each lived long enough to count as
        // legitimate, so the wait bounds out as LockTimeout, not Provider.
        waiter_backend
            .lock("ledger", Duration::from_secs(1000), Duration::from_secs(5))
            .await
    });
    let Ok(joined) = waiter.await else {
        panic!("waiter task must join");
    };
    assert!(
        matches!(joined, Err(ClusterError::LockTimeout { ref name, .. }) if name == "ledger"),
        "legitimate watch rotations must not trip the unusable-watch cap; got {joined:?}"
    );
}

#[tokio::test]
async fn revoke_resolves_an_in_flight_waiter_to_shutdown_not_timeout() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("construct");
    };
    let backend = Arc::new(backend);
    // Hold the lock with a long timeout so the waiter genuinely blocks and would
    // otherwise wait the full acquisition timeout, not time out promptly.
    let Ok(_held) = backend.try_lock("ledger", Duration::from_secs(100)).await else {
        panic!("first holder acquires");
    };
    let waiter_backend = Arc::clone(&backend);
    let waiter = tokio::spawn(async move {
        waiter_backend
            .lock("ledger", Duration::from_secs(100), Duration::from_secs(100))
            .await
    });
    // Let the waiter reach the watch wait, then revoke the backend.
    settle().await;
    backend.revoke().await;
    let Ok(joined) = waiter.await else {
        panic!("waiter task must join");
    };
    // The waiter resolves to Shutdown — NOT a LockTimeout — promptly.
    assert!(
        matches!(joined, Err(ClusterError::Shutdown)),
        "an in-flight waiter must observe Shutdown on revoke, not LockTimeout; got {joined:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn blocking_lock_times_out_when_held() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("construct");
    };
    let backend = Arc::new(backend);
    // Hold the lock with a long TTL so it is never reaped during the test.
    let Ok(_held) = backend.try_lock("ledger", Duration::from_secs(100)).await else {
        panic!("hold the lock");
    };
    let waiter_backend = Arc::clone(&backend);
    let waiter = tokio::spawn(async move {
        waiter_backend
            .lock("ledger", Duration::from_secs(100), Duration::from_secs(1))
            .await
    });
    // Advance past the acquisition timeout.
    tokio::time::advance(Duration::from_secs(2)).await;
    let Ok(joined) = waiter.await else {
        panic!("waiter task must join");
    };
    assert!(matches!(
        joined,
        Err(ClusterError::LockTimeout { name, .. }) if name == "ledger"
    ));
}

// The lease-token half of the trait (§5.8.1, item `L1`)

/// Two backends over one cache — the in-process stand-in for two cluster replicas
/// over one backing store.
fn two_handles(
    cache: &Arc<MemoryCache>,
) -> (
    CasBasedDistributedLockBackend,
    CasBasedDistributedLockBackend,
) {
    // Virtual clock on both handles: these two-replica tests lapse leases under
    // `tokio::time::advance`, which the production wall clock (H3) never moves.
    let build = || {
        CasBasedDistributedLockBackend::new(Arc::clone(cache) as Arc<dyn ClusterCacheBackend>)
            .expect("a linearizable cache must construct")
            .with_virtual_clock()
    };
    (build(), build())
}

#[tokio::test]
async fn acquire_returns_the_token_and_contends_while_held() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("construct");
    };
    let Ok(token) = backend
        .acquire("ledger", "owner-a", Duration::from_secs(30))
        .await
    else {
        panic!("a free lock must acquire");
    };
    assert_eq!(token.name, "ledger");
    assert_eq!(token.owner, "owner-a");
    assert_eq!(token.fence, 1);
    assert!(matches!(
        backend.acquire("ledger", "owner-b", Duration::from_secs(30)).await,
        Err(ClusterError::LockContended { name }) if name == "ledger"
    ));
}

#[tokio::test]
async fn a_lease_is_renewable_and_releasable_through_another_backend_handle() {
    // `L1` exit criterion, at the trait boundary: the answer to a lease operation
    // is a property of the record, so a handle that never saw the acquire serves it
    // identically (invariant I7).
    let cache = MemoryCache::linearizable();
    let (acquirer, other_replica) = two_handles(&cache);

    let Ok(token) = acquirer
        .acquire("ledger", "owner-a", Duration::from_secs(30))
        .await
    else {
        panic!("acquire");
    };
    assert!(
        other_replica
            .renew(&token, Duration::from_mins(1))
            .await
            .is_ok(),
        "a replica that never saw the acquire must serve the renew"
    );
    assert!(other_replica.release(&token).await.is_ok());
    // The lock is free again, from either handle's point of view.
    assert!(
        acquirer
            .acquire("ledger", "owner-b", Duration::from_secs(30))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn renew_through_another_handle_refuses_a_non_matching_token() {
    let cache = MemoryCache::linearizable();
    let (acquirer, other_replica) = two_handles(&cache);
    let Ok(token) = acquirer
        .acquire("ledger", "owner-a", Duration::from_secs(30))
        .await
    else {
        panic!("acquire");
    };

    let wrong_owner = LeaseToken::new(&token.name, "owner-b", token.fence);
    let wrong_fence = LeaseToken::new(&token.name, &token.owner, token.fence + 1);
    assert!(matches!(
        other_replica.renew(&wrong_owner, Duration::from_secs(30)).await,
        Err(ClusterError::LockExpired { name }) if name == "ledger"
    ));
    assert!(matches!(
        other_replica
            .renew(&wrong_fence, Duration::from_secs(30))
            .await,
        Err(ClusterError::LockExpired { .. })
    ));
    // Neither attempt disturbed the real holder.
    assert!(
        other_replica
            .renew(&token, Duration::from_secs(30))
            .await
            .is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn renew_through_another_handle_refuses_a_lapsed_lease() {
    let cache = MemoryCache::linearizable();
    let (acquirer, other_replica) = two_handles(&cache);
    let Ok(token) = acquirer
        .acquire("ledger", "owner-a", Duration::from_secs(5))
        .await
    else {
        panic!("acquire");
    };
    tokio::time::advance(Duration::from_secs(6)).await;
    assert!(matches!(
        other_replica.renew(&token, Duration::from_secs(5)).await,
        Err(ClusterError::LockExpired { .. })
    ));
}

#[tokio::test]
async fn release_of_a_lease_nobody_holds_is_ok() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("construct");
    };
    assert!(
        backend
            .release(&LeaseToken::new("ledger", "owner-a", 1))
            .await
            .is_ok(),
        "releasing an absent lease is success, not an error (DESIGN 6.10)"
    );
}

#[tokio::test(start_paused = true)]
async fn acquiring_an_expired_lock_strictly_increases_the_fence() {
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache)
        .map(CasBasedDistributedLockBackend::with_virtual_clock)
    else {
        panic!("construct");
    };
    let Ok(first) = backend
        .acquire("ledger", "owner-a", Duration::from_secs(5))
        .await
    else {
        panic!("acquire");
    };
    tokio::time::advance(Duration::from_secs(6)).await;
    let Ok(second) = backend
        .acquire("ledger", "owner-b", Duration::from_secs(5))
        .await
    else {
        panic!("an expired lease must be stealable");
    };
    assert!(
        second.fence > first.fence,
        "{} !> {}",
        second.fence,
        first.fence
    );
}

#[tokio::test]
async fn a_guard_and_a_token_are_the_same_lease() {
    // The guard path is built over the token path, so a lock held through
    // `try_lock` contends against a token acquisition and vice versa.
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache) else {
        panic!("construct");
    };
    let Ok(_guard) = backend.try_lock("ledger", Duration::from_secs(30)).await else {
        panic!("acquire");
    };
    assert!(matches!(
        backend
            .acquire("ledger", "owner-a", Duration::from_secs(30))
            .await,
        Err(ClusterError::LockContended { .. })
    ));

    let Ok(token) = backend
        .acquire("other", "owner-a", Duration::from_secs(30))
        .await
    else {
        panic!("acquire");
    };
    assert!(matches!(
        backend.try_lock("other", Duration::from_secs(30)).await,
        Err(ClusterError::LockContended { .. })
    ));
    assert!(backend.release(&token).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn acquire_waiting_takes_the_lease_when_the_incumbent_lapses() {
    // A lapsing lease writes nothing, so no watch event announces it: the waiter
    // has to wake itself at the incumbent's deadline.
    let cache = MemoryCache::linearizable();
    let Ok(backend) = CasBasedDistributedLockBackend::new(cache)
        .map(CasBasedDistributedLockBackend::with_virtual_clock)
    else {
        panic!("construct");
    };
    let Ok(held) = backend
        .acquire("ledger", "owner-a", Duration::from_secs(5))
        .await
    else {
        panic!("acquire");
    };
    // The holder "crashes" — no release, no renewal.
    let waiter = tokio::spawn({
        let backend = Arc::new(backend);
        let handle = Arc::clone(&backend);
        async move {
            handle
                .acquire_waiting(
                    "ledger",
                    "owner-b",
                    Duration::from_secs(5),
                    Duration::from_secs(30),
                )
                .await
        }
    });
    tokio::time::advance(Duration::from_secs(6)).await;
    settle().await;
    let Ok(Ok(taken)) = waiter.await else {
        panic!("the waiter must take the lease once it lapses");
    };
    assert!(
        taken.fence > held.fence,
        "and it must fence its predecessor"
    );
}
