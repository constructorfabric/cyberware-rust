#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::{CacheConfig, EventInvalidationConfig};
use crate::domain::clock::StubClock;

fn cache(
    ttl_seconds: u64,
    max_entries: usize,
    singleflight: bool,
) -> (Arc<HierarchyCache>, Arc<StubClock>) {
    let config = CacheConfig {
        ttl_seconds,
        max_entries: std::num::NonZeroUsize::new(max_entries)
            .expect("cache fixtures must pass a non-zero capacity"),
        singleflight_enabled: singleflight,
        event_invalidation: EventInvalidationConfig::default(),
    };
    let clock = Arc::new(StubClock::new());
    let metrics = Arc::new(AuthZMetrics::from_global());
    let cache = Arc::new(HierarchyCache::new(
        &config,
        Arc::clone(&clock) as Arc<dyn Clock>,
        metrics,
    ));
    (cache, clock)
}

fn key(id: u128) -> CacheKey {
    CacheKey::TenantMeta {
        id: Uuid::from_u128(id),
    }
}

fn meta(id: u128) -> CacheValue {
    CacheValue::TenantMeta(TenantMetadata {
        id: Uuid::from_u128(id),
        status: TenantStatus::Active,
        self_managed: false,
        parent_id: None,
    })
}

#[tokio::test]
async fn u_40_cache_hit_does_not_invoke_fetch() {
    let (cache, _clock) = cache(60, 10, true);
    let counter = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&counter);
    let v = meta(1);

    // Populate by calling get_or_fetch once.
    let first = cache
        .get_or_fetch(key(1), || async {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(v.clone())
        })
        .await
        .unwrap();
    let _ = first;
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // Second call must hit the cache and NOT invoke the fetch closure.
    let _second = cache
        .get_or_fetch(key(1), || async {
            panic!("fetch must NOT be invoked on cache hit");
        })
        .await
        .unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn u_41_cache_miss_populates_then_hits() {
    let (cache, _clock) = cache(60, 10, true);
    let counter = Arc::new(AtomicUsize::new(0));

    let c1 = Arc::clone(&counter);
    cache
        .get_or_fetch(key(2), move || async move {
            c1.fetch_add(1, Ordering::SeqCst);
            Ok(meta(2))
        })
        .await
        .unwrap();

    let c2 = Arc::clone(&counter);
    cache
        .get_or_fetch(key(2), move || async move {
            c2.fetch_add(1, Ordering::SeqCst);
            Ok(meta(2))
        })
        .await
        .unwrap();

    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn u_42_ttl_expiration_triggers_refetch() {
    let (cache, clock) = cache(60, 10, true);
    let counter = Arc::new(AtomicUsize::new(0));

    let c1 = Arc::clone(&counter);
    cache
        .get_or_fetch(key(3), move || async move {
            c1.fetch_add(1, Ordering::SeqCst);
            Ok(meta(3))
        })
        .await
        .unwrap();

    clock.advance(Duration::from_secs(61));

    let c2 = Arc::clone(&counter);
    cache
        .get_or_fetch(key(3), move || async move {
            c2.fetch_add(1, Ordering::SeqCst);
            Ok(meta(3))
        })
        .await
        .unwrap();

    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "expired entry must trigger a fresh fetch"
    );
}

#[tokio::test]
async fn u_43_lru_eviction_at_capacity() {
    let (cache, _clock) = cache(60, 3, true);

    for i in 1..=3 {
        cache
            .get_or_fetch(key(i), || async move { Ok(meta(i)) })
            .await
            .unwrap();
    }
    // Touch key(1) so it becomes most-recently-used; key(2) is now LRU.
    cache
        .get_or_fetch(key(1), || async {
            panic!("hit expected");
        })
        .await
        .unwrap();

    // Insert a fourth key — key(2) must be evicted.
    cache
        .get_or_fetch(key(4), || async move { Ok(meta(4)) })
        .await
        .unwrap();

    // key(2) is gone — a new get_or_fetch invokes the fetch again.
    let counter = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&counter);
    cache
        .get_or_fetch(key(2), move || async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(meta(2))
        })
        .await
        .unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn u_44_singleflight_enabled_coalesces_concurrent_misses() {
    let (cache, _clock) = cache(60, 10, true);
    let counter = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(10));

    let mut handles = Vec::new();
    for _ in 0..10 {
        let cache = Arc::clone(&cache);
        let counter = Arc::clone(&counter);
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            cache
                .get_or_fetch(key(7), move || async move {
                    // Hold for a tick so all waiters land before completion.
                    tokio::task::yield_now().await;
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(meta(7))
                })
                .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "singleflight must coalesce all 10 concurrent misses into one fetch"
    );
}

/// One failed fetch is shared with every singleflight waiter — the reason
/// `PluginError` is `Clone` at all.
///
/// The leader's error must reach the waiters as the SAME error, and it must
/// NOT be cached: the next call has to retry. A `Clone` derive compiling
/// proves neither, which is all the error module's own test could observe.
#[tokio::test]
async fn singleflight_waiters_all_receive_the_leader_s_error() {
    let (cache, _clock) = cache(60, 10, true);
    let fetches = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(8));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let cache = Arc::clone(&cache);
        let fetches = Arc::clone(&fetches);
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            cache
                .get_or_fetch(key(11), move || async move {
                    tokio::task::yield_now().await;
                    fetches.fetch_add(1, Ordering::SeqCst);
                    Err(PluginError::TenantResolverUnavailable)
                })
                .await
        }));
    }

    let mut outcomes = Vec::new();
    for h in handles {
        outcomes.push(h.await.unwrap());
    }

    assert_eq!(
        fetches.load(Ordering::SeqCst),
        1,
        "the failing fetch must still be coalesced into one upstream call"
    );
    for (i, outcome) in outcomes.iter().enumerate() {
        match outcome {
            Err(err) => assert_eq!(
                *err,
                PluginError::TenantResolverUnavailable,
                "waiter {i} must receive the leader's error verbatim"
            ),
            Ok(value) => panic!("waiter {i} must observe the failure, got Ok({value:?})"),
        }
    }

    // An error is never cached, so a later call retries rather than serving a
    // poisoned entry for a full TTL.
    cache
        .get_or_fetch(key(11), || async { Ok(meta(11)) })
        .await
        .expect("a retry after a shared failure must reach upstream again");
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        1,
        "the retry ran its own closure; the failure was not cached"
    );
}

#[tokio::test]
async fn u_45_singleflight_disabled_independent_fetches() {
    let (cache, _clock) = cache(60, 10, false);
    let counter = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let mut handles = Vec::new();
    for _ in 0..2 {
        let cache = Arc::clone(&cache);
        let counter = Arc::clone(&counter);
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            cache
                .get_or_fetch(key(8), move || async move {
                    tokio::task::yield_now().await;
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(meta(8))
                })
                .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }

    // With singleflight off, both tasks see an empty cache and fetch
    // independently. The `yield_now().await` inside the closure guarantees
    // both fetches start before either completes, so both increments land.
    // The whole point of this test is to prove no coalescing happens — `>= 1`
    // would pass even if singleflight silently regressed to coalescing, so
    // pin the exact count.
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "singleflight disabled - both fetches must run independently"
    );
}

#[tokio::test]
async fn leader_error_propagates_and_not_cached() {
    let (cache, _clock) = cache(60, 10, true);
    let counter = Arc::new(AtomicUsize::new(0));

    // First call fails.
    let c = Arc::clone(&counter);
    let first = cache
        .get_or_fetch(key(9), move || async move {
            c.fetch_add(1, Ordering::SeqCst);
            Err(PluginError::internal("first failure"))
        })
        .await;
    assert!(first.is_err());
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // Second call must invoke the fetch again — error was not cached.
    let c = Arc::clone(&counter);
    let second = cache
        .get_or_fetch(key(9), move || async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(meta(9))
        })
        .await;
    assert!(second.is_ok());
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn late_waiter_observes_published_result_without_hanging() {
    // A waiter that subscribes mid-flight but only checks the channel AFTER
    // the leader has published and released must still observe the value:
    // `watch` latches it, so there is no signal to miss and the waiter cannot
    // block forever.
    let (cache, _clock) = cache(60, 10, true);
    let k = key(42);

    // Leader registers the flight; waiter subscribes mid-flight.
    let Lease::Leader(mut lease) = cache.acquire_lease(k) else {
        panic!("first acquire_lease must be leader");
    };
    let Lease::Waiter(rx) = cache.acquire_lease(k) else {
        panic!("second acquire_lease must be waiter");
    };

    // Leader publishes and drops (guard removes the entry) BEFORE the
    // waiter looks at the channel — the ordering that deadlocked `Notify`.
    // The value stays latched in the channel even after the sender drops.
    lease.publish(FlightState::Done(Ok(std::sync::Arc::new(meta(42)))));
    drop(lease);

    // The latched value is visible without ever having parked first.
    let borrowed = rx.borrow();
    let FlightState::Done(Ok(value)) = &*borrowed else {
        panic!("expected a published value, got {borrowed:?}");
    };
    let CacheValue::TenantMeta(m) = &**value else {
        panic!("expected published TenantMeta, got {borrowed:?}");
    };
    assert_eq!(m.id, Uuid::from_u128(42));
}

#[test]
fn leader_lease_drop_abandons_and_cleans_up() {
    // A leader cancelled/panicked before publishing must not wedge the
    // key. Dropping the lease (simulating cancellation) sends `Abandoned`
    // to waiters and removes the in_flight entry so the next caller is a
    // fresh leader — no permanent loss of coalescing, no map leak.
    let (cache, _clock) = cache(60, 10, true);
    let k = key(99);

    let Lease::Leader(lease) = cache.acquire_lease(k) else {
        panic!("first acquire_lease must be leader");
    };
    let Lease::Waiter(rx) = cache.acquire_lease(k) else {
        panic!("second acquire_lease must be waiter");
    };

    drop(lease); // cancellation before publish

    // Waiter observes a terminal `Abandoned` (latched), not a hang.
    assert!(matches!(&*rx.borrow(), FlightState::Abandoned));
    // Entry was cleaned up → the next acquire is a fresh leader.
    assert!(
        matches!(cache.acquire_lease(k), Lease::Leader(_)),
        "in_flight entry must be removed when the leader drops without publishing"
    );
}

#[tokio::test]
async fn leader_failure_is_shared_no_stampede() {
    // When the leader's fetch fails, waiters share the (cloned) error
    // instead of each re-fetching. With 8 concurrent callers on the same
    // key, exactly one fetch runs — no thundering herd against a failing
    // downstream.
    let (cache, _clock) = cache(60, 10, true);
    let counter = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(8));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let cache = Arc::clone(&cache);
        let counter = Arc::clone(&counter);
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            cache
                .get_or_fetch(key(123), move || async move {
                    tokio::task::yield_now().await;
                    counter.fetch_add(1, Ordering::SeqCst);
                    Err::<CacheValue, _>(PluginError::internal("downstream down"))
                })
                .await
        }));
    }
    for h in handles {
        let result = h.await.unwrap();
        // Every caller sees the shared error.
        assert!(matches!(
            result,
            Err(PluginError::Internal { detail: ref m }) if m == "downstream down"
        ));
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "leader failure must be shared with waiters - exactly one fetch, no stampede"
    );
}

#[tokio::test]
async fn abandoned_leader_waiters_recoalesce_no_stampede() {
    // When the leader is CANCELLED before publishing (Abandoned), the woken
    // waiters must RE-COALESCE — one becomes a new leader, the rest wait on it —
    // rather than each running its own fetch. Otherwise a cancellation storm
    // becomes the thundering herd singleflight exists to prevent. Assert exactly
    // one fetch runs after the abandon.
    let (cache, _clock) = cache(60, 10, true);
    let k = key(0xABA0);
    let counter = Arc::new(AtomicUsize::new(0));

    // Manually hold the leader lease so the in_flight entry exists; the spawned
    // callers below then register as real waiters on it via get_or_fetch.
    let Lease::Leader(lease) = cache.acquire_lease(k) else {
        panic!("first acquire_lease must be leader");
    };

    let mut handles = Vec::new();
    for _ in 0..6 {
        let cache = Arc::clone(&cache);
        let counter = Arc::clone(&counter);
        handles.push(tokio::spawn(async move {
            cache
                .get_or_fetch(k, move || async move {
                    tokio::task::yield_now().await;
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(meta(0xABA0))
                })
                .await
        }));
    }

    // Let the spawned callers reach the parked waiter state (rx.changed()).
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }

    // Cancel the leader before it publishes → Drop sends `Abandoned`.
    drop(lease);

    for h in handles {
        let result = h.await.unwrap();
        assert!(
            result.is_ok(),
            "every waiter must resolve to the re-coalesced value, not hang"
        );
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "abandoned-leader waiters must re-coalesce to exactly one fetch, not stampede"
    );
}

#[test]
fn hash_ids_is_order_independent() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    assert_eq!(hash_ids(&[a, b]), hash_ids(&[b, a]));
}

#[test]
fn hash_ids_dedups() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    assert_eq!(hash_ids(&[a, a, b]), hash_ids(&[a, b]));
}

#[test]
fn hash_status_is_order_independent() {
    let a = [TenantStatus::Active, TenantStatus::Suspended];
    let b = [TenantStatus::Suspended, TenantStatus::Active];
    assert_eq!(hash_status(&a), hash_status(&b));
}

// The equality tests above pin what must COLLAPSE (order, duplicates). These
// pin what must stay APART. Without them a hash that ignored its input — a
// refactor returning a constant, say — would satisfy the whole suite while
// collapsing every `CacheKey::GroupSubtree` / `TenantSubtree` entry into one,
// serving one group set's materialization for another's.
#[test]
fn hash_ids_distinguishes_different_id_sets() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    assert_ne!(hash_ids(&[a]), hash_ids(&[b]), "different ids");
    assert_ne!(hash_ids(&[a]), hash_ids(&[a, b]), "superset of the same id");
    assert_ne!(hash_ids(&[]), hash_ids(&[a]), "empty vs non-empty");
}

#[test]
fn hash_status_distinguishes_different_filters() {
    assert_ne!(
        hash_status(&[TenantStatus::Active]),
        hash_status(&[TenantStatus::Suspended]),
        "different statuses"
    );
    assert_ne!(
        hash_status(&[]),
        hash_status(&[TenantStatus::Active]),
        "no filter vs Active-only"
    );
    assert_ne!(
        hash_status(&[TenantStatus::Active]),
        hash_status(&[TenantStatus::Active, TenantStatus::Suspended]),
        "superset of the same status"
    );
}
