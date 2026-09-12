// Created: 2026-06-24 by Constructor Tech
//! Cache conformance scenarios (`SC-CACHE-*`).
//!
//! See the [scenario catalog](../docs/scenarios/cache.md) for per-scenario
//! intent/steps/expected/done-when. Each `scenario_cache_*` is a `pub async fn`
//! a plugin may call directly; [`run_cache_conformance`] drives the full L2 set,
//! building a fresh backend per scenario via `make` so state never leaks.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use cluster_sdk::cache::{
    CacheConsistency, CacheEvent, ClusterCacheBackend, PollingPrefixWatch, PutRequest, Ttl,
};
use cluster_sdk::error::ClusterError;

use crate::factory::{ScenarioBackend, run_scenario};
use crate::time::TimeControl;
use crate::watch_poll::poll_watch;

/// Runs every implemented L2 cache scenario, each against a fresh backend built
/// by the async factory `make` and torn down afterward (see
/// [`ScenarioBackend`]). `time` selects the clock model
/// ([`TimeControl::Virtual`] for in-memory fixtures, [`TimeControl::Real`] for a
/// real-I/O backend — see [`crate::time`]).
///
/// Capability-gated scenarios read `consistency()`/`features()` off the backend
/// and assert strict guarantees only where the backend claims them.
pub async fn run_cache_conformance<F, Fut>(make: F, time: TimeControl)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ScenarioBackend<dyn ClusterCacheBackend>>,
{
    run_scenario(make(), scenario_cache_001).await;
    run_scenario(make(), scenario_cache_002).await;
    run_scenario(make(), scenario_cache_003).await;
    run_scenario(make(), scenario_cache_004).await;
    run_scenario(make(), scenario_cache_005).await;
    run_scenario(make(), scenario_cache_006).await;
    run_scenario(make(), scenario_cache_007).await;
    run_scenario(make(), scenario_cache_008).await;
    run_scenario(make(), scenario_cache_009).await;
    run_scenario(make(), |b| scenario_cache_010(b, time)).await;
    run_scenario(make(), |b| scenario_cache_011(b, time)).await;
    run_scenario(make(), scenario_cache_012).await;
    run_scenario(make(), scenario_cache_013).await;
    run_scenario(make(), |b| scenario_cache_014(b, time)).await;
    run_scenario(make(), |b| scenario_cache_015(b, time)).await;
}

/// SC-CACHE-001: `get` on an absent key returns `Ok(None)`, never an error.
pub async fn scenario_cache_001(backend: Arc<dyn ClusterCacheBackend>) {
    let got = backend.get("absent").await;
    assert!(
        matches!(got, Ok(None)),
        "SC-CACHE-001: get on absent key must be Ok(None), got {got:?}"
    );
}

/// SC-CACHE-002: `put` then `get` returns the stored value at version 1.
pub async fn scenario_cache_002(backend: Arc<dyn ClusterCacheBackend>) {
    backend
        .put(PutRequest {
            key: "k",
            value: b"v",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put must succeed");
    let entry = backend
        .get("k")
        .await
        .expect("get must succeed")
        .expect("entry must be present");
    assert_eq!(
        entry.value, b"v",
        "SC-CACHE-002: stored value must round-trip"
    );
    assert_eq!(entry.version, 1, "SC-CACHE-002: first write is version 1");
}

/// SC-CACHE-003: each overwrite strictly increments the version; version 0 is
/// never observed (it is the reserved sentinel).
pub async fn scenario_cache_003(backend: Arc<dyn ClusterCacheBackend>) {
    backend
        .put(PutRequest {
            key: "k",
            value: b"a",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put");
    let v1 = backend
        .get("k")
        .await
        .expect("get")
        .expect("present")
        .version;
    backend
        .put(PutRequest {
            key: "k",
            value: b"b",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put");
    let v2 = backend
        .get("k")
        .await
        .expect("get")
        .expect("present")
        .version;
    assert_ne!(v1, 0, "SC-CACHE-003: version 0 is reserved as a sentinel");
    assert!(
        v2 > v1,
        "SC-CACHE-003: each overwrite must strictly increment version ({v1} -> {v2})"
    );
}

/// SC-CACHE-004: `put_if_absent` returns `Some(entry)` on create, `None` when
/// already present (atomic create).
pub async fn scenario_cache_004(backend: Arc<dyn ClusterCacheBackend>) {
    let created = backend
        .put_if_absent(PutRequest {
            key: "k",
            value: b"a",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("pia");
    assert!(created.is_some(), "SC-CACHE-004: first create returns Some");
    let again = backend
        .put_if_absent(PutRequest {
            key: "k",
            value: b"b",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("pia");
    assert!(
        again.is_none(),
        "SC-CACHE-004: create when present returns None"
    );
}

/// SC-CACHE-005: `compare_and_swap` succeeds iff `expected_version == current`.
pub async fn scenario_cache_005(backend: Arc<dyn ClusterCacheBackend>) {
    let created = backend
        .put_if_absent(PutRequest {
            key: "k",
            value: b"a",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("pia")
        .expect("created");
    let swapped = backend
        .compare_and_swap("k", created.version, b"b", Ttl::Indefinite)
        .await
        .expect("CAS at current version must succeed");
    assert_eq!(
        swapped.value, b"b",
        "SC-CACHE-005: CAS applies the new value"
    );
    assert!(
        swapped.version > created.version,
        "SC-CACHE-005: successful CAS bumps the version"
    );
}

/// SC-CACHE-006: CAS on a stale version returns `CasConflict { key, current }`
/// carrying the current entry.
pub async fn scenario_cache_006(backend: Arc<dyn ClusterCacheBackend>) {
    let created = backend
        .put_if_absent(PutRequest {
            key: "k",
            value: b"a",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("pia")
        .expect("created");
    // Use an unambiguously stale version (far ahead of 1) — avoids the reserved
    // sentinel 0 that wrapping_sub(1) would produce for a freshly-created entry.
    let stale = created.version.wrapping_add(100);
    let err = backend
        .compare_and_swap("k", stale, b"b", Ttl::Indefinite)
        .await
        .expect_err("CAS on a stale version must conflict");
    match err {
        ClusterError::CasConflict { key, current } => {
            assert_eq!(key, "k", "SC-CACHE-006: conflict reports the key");
            let current = current.expect("SC-CACHE-006: conflict carries the current entry");
            assert_eq!(
                current.version, created.version,
                "SC-CACHE-006: conflict carries the live version"
            );
        }
        other => panic!("SC-CACHE-006: expected CasConflict, got {other:?}"),
    }
}

/// SC-CACHE-007: `delete` removes the entry and reports prior existence.
pub async fn scenario_cache_007(backend: Arc<dyn ClusterCacheBackend>) {
    backend
        .put(PutRequest {
            key: "k",
            value: b"v",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put");
    assert!(
        backend.delete("k").await.expect("delete"),
        "SC-CACHE-007: deleting a present key reports true"
    );
    assert!(
        backend.get("k").await.expect("get").is_none(),
        "SC-CACHE-007: deleted key reads as absent"
    );
    assert!(
        !backend.delete("k").await.expect("delete"),
        "SC-CACHE-007: deleting an absent key reports false"
    );
}

/// SC-CACHE-008: `compare_and_delete` removes only when the owner token matches;
/// a mismatch or absent key returns `Ok(false)`, never an error.
pub async fn scenario_cache_008(backend: Arc<dyn ClusterCacheBackend>) {
    backend
        .put(PutRequest {
            key: "k",
            value: b"owner-a",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put");
    assert!(
        !backend
            .compare_and_delete("k", b"owner-b")
            .await
            .expect("c&d must not error on mismatch"),
        "SC-CACHE-008: mismatched owner token must not delete"
    );
    assert!(
        backend
            .compare_and_delete("k", b"owner-a")
            .await
            .expect("c&d"),
        "SC-CACHE-008: matching owner token deletes"
    );
    assert!(
        !backend
            .compare_and_delete("absent", b"x")
            .await
            .expect("c&d on absent must be Ok(false)"),
        "SC-CACHE-008: absent key returns Ok(false)"
    );
}

/// SC-CACHE-009: a key deleted and re-created resets to version 1, and a
/// value/owner guard still distinguishes the successor — a named regression for
/// the version-reset caveat (a version guard would alias a successor's fresh
/// claim).
pub async fn scenario_cache_009(backend: Arc<dyn ClusterCacheBackend>) {
    // Holder A claims, then its claim lapses and it is deleted.
    backend
        .put(PutRequest {
            key: "lock",
            value: b"holder-a",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put");
    backend.delete("lock").await.expect("delete");
    // Successor B re-creates the key — version resets to 1.
    let b = backend
        .put_if_absent(PutRequest {
            key: "lock",
            value: b"holder-b",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("pia")
        .expect("created");
    assert_eq!(b.version, 1, "SC-CACHE-009: re-create resets version to 1");
    // A's late guarded release must NOT remove B's fresh claim.
    assert!(
        !backend
            .compare_and_delete("lock", b"holder-a")
            .await
            .expect("c&d"),
        "SC-CACHE-009: stale owner token must not delete the successor's claim"
    );
    assert_eq!(
        backend
            .get("lock")
            .await
            .expect("get")
            .expect("present")
            .value,
        b"holder-b",
        "SC-CACHE-009: successor's claim survives the stale release"
    );
}

/// SC-CACHE-010: TTL expiry removes the entry, and — on a backend with exact
/// watch — emits `CacheEvent::Expired` to watchers. The finite-TTL removal check
/// (`get` reads absent after the deadline) runs for *every* backend; only the
/// watcher subscription and the `Expired` assertion are gated on
/// `features().watch` (a watchless backend has no watcher to emit to, and the
/// `watch()` contract itself is asserted by SC-CACHE-012). Expiry is observed
/// lazily on read, so the removal assertion holds after the elapse without
/// depending on a background sweep.
pub async fn scenario_cache_010(backend: Arc<dyn ClusterCacheBackend>, time: TimeControl) {
    time.begin();
    // Subscribe before the put so the watcher is guaranteed to see the Expired
    // emission. Watchless backends skip this and assert removal only.
    let mut watch = if backend.features().watch {
        Some(backend.watch("k").await.expect("watch"))
    } else {
        None
    };
    backend
        .put(PutRequest {
            key: "k",
            value: b"v",
            ttl: Ttl::Of(Duration::from_millis(50)),
        })
        .await
        .expect("put with ttl");
    time.elapse(Duration::from_millis(100)).await;
    if let Some(watch) = watch.as_mut() {
        // Drain the initial Changed, then wait for the Expired emission. The poll
        // keeps reading, so under `Real` it tolerates the sweeper reclaim landing
        // a little after the elapse (caller must set a sweep interval < the elapse).
        let expired = poll_watch(watch)
            .until(|event| matches!(event, CacheEvent::Expired { key } if key == "k"))
            .await;
        assert!(expired, "SC-CACHE-010: TTL expiry must emit Expired");
    }
    assert!(
        backend.get("k").await.expect("get").is_none(),
        "SC-CACHE-010: expired entry reads as absent"
    );
    time.end();
}

/// SC-CACHE-012: exact `watch` yields `Changed`/`Deleted` for the key,
/// preserving per-key order — and a backend that declares no exact watch
/// (`features().watch == false`) returns `Unsupported` instead of a channel that
/// never fires, the exact-watch twin of SC-CACHE-013.
pub async fn scenario_cache_012(backend: Arc<dyn ClusterCacheBackend>) {
    /// Which of the two ordered events on key `k` a poll selected.
    enum Seen {
        Changed,
        Deleted,
    }

    if !backend.features().watch {
        assert!(
            matches!(
                backend.watch("k").await,
                Err(ClusterError::Unsupported { feature: "watch" })
            ),
            "SC-CACHE-012: a backend without exact watch must return Unsupported"
        );
        return;
    }

    let mut watch = backend.watch("k").await.expect("watch");
    backend
        .put(PutRequest {
            key: "k",
            value: b"v",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put");
    backend.delete("k").await.expect("delete");
    // Select on *either* event, so the order is what the two polls assert: the
    // first must be the Changed, the second the Deleted. A predicate that only
    // looked for Changed would skip a too-early Deleted instead of failing.
    let on_key = |event| match event {
        CacheEvent::Changed { key } if key == "k" => Some(Seen::Changed),
        CacheEvent::Deleted { key } if key == "k" => Some(Seen::Deleted),
        _ => None,
    };
    match poll_watch(&mut watch)
        .first_match(on_key)
        .await
        .expect_match("SC-CACHE-012: no Changed event for the key")
    {
        Seen::Changed => {}
        Seen::Deleted => {
            panic!("SC-CACHE-012: Deleted arrived before Changed -- order violated")
        }
    }
    match poll_watch(&mut watch)
        .first_match(on_key)
        .await
        .expect_match("SC-CACHE-012: no Deleted event after the Changed")
    {
        Seen::Deleted => {}
        Seen::Changed => panic!("SC-CACHE-012: duplicate Changed event"),
    }
}

/// SC-CACHE-013: `watch_prefix` yields events for matching keys when the backend
/// declares the `prefix_watch` feature; backends without it return
/// `Unsupported` (capability-gated).
pub async fn scenario_cache_013(backend: Arc<dyn ClusterCacheBackend>) {
    let watch = backend.watch_prefix("p/").await;
    if backend.features().prefix_watch {
        let mut watch = watch.expect("SC-CACHE-013: prefix-watch backend must establish a watch");
        backend
            .put(PutRequest {
                key: "p/a",
                value: b"v",
                ttl: Ttl::Indefinite,
            })
            .await
            .expect("put");
        assert!(
            poll_watch(&mut watch)
                .until(|e| matches!(e, CacheEvent::Changed { key } if key == "p/a"))
                .await,
            "SC-CACHE-013: prefix watch yields events for matching keys"
        );
    } else {
        assert!(
            matches!(
                watch,
                Err(ClusterError::Unsupported {
                    feature: "prefix_watch"
                })
            ),
            "SC-CACHE-013: backend without prefix_watch must return Unsupported"
        );
    }
}

/// SC-CACHE-011: a `Ttl::Indefinite` entry persists well past any default TTL;
/// it is not removed by the TTL sweeper until explicitly deleted.
pub async fn scenario_cache_011(backend: Arc<dyn ClusterCacheBackend>, time: TimeControl) {
    time.begin();
    backend
        .put(PutRequest {
            key: "k",
            value: b"v",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put");
    // A large advance under `Virtual`; under `Real` this is capped to a short
    // real sleep (several sweep intervals) — enough for a sweeper to run and
    // (incorrectly) remove the entry if it mishandled the indefinite TTL.
    time.elapse(Duration::from_hours(1)).await;
    let entry = backend.get("k").await.expect("get");
    assert!(
        entry.is_some(),
        "SC-CACHE-011: an indefinite entry must survive a large time advance"
    );
    time.end();
}

/// SC-CACHE-014: `PollingPrefixWatch` synthesizes `Changed`/`Deleted` diffs for
/// a backend that does not natively support prefix watches.
/// Capability-gated on `!features().prefix_watch`.
pub async fn scenario_cache_014(backend: Arc<dyn ClusterCacheBackend>, time: TimeControl) {
    if backend.features().prefix_watch {
        return; // native prefix watch; polyfill is not the subject here
    }
    time.begin();
    let mut watch = PollingPrefixWatch::spawn(backend.clone(), "p/", Duration::from_millis(25));

    // A put under the prefix must be diffed as Changed.
    backend
        .put(PutRequest {
            key: "p/a",
            value: b"1",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put p/a");
    time.elapse(Duration::from_millis(50)).await;
    assert!(
        poll_watch(&mut watch)
            .until(|e| matches!(e, CacheEvent::Changed { key } if key == "p/a"))
            .await,
        "SC-CACHE-014: put under prefix must yield a Changed event"
    );

    // A delete must be diffed as Deleted.
    backend.delete("p/a").await.expect("delete p/a");
    time.elapse(Duration::from_millis(50)).await;
    assert!(
        poll_watch(&mut watch)
            .until(|e| matches!(e, CacheEvent::Deleted { key } if key == "p/a"))
            .await,
        "SC-CACHE-014: delete under prefix must yield a Deleted event"
    );
    time.end();
}

/// SC-CACHE-015: watch delivery is at-most-once per mutation — a single `put`
/// must not cause more than one `Changed` event for the same key on one watch.
/// Capability-gated on `features().watch`: at-most-once delivery is a watch
/// property, so a watchless backend (SC-CACHE-012 asserts its `Unsupported`
/// contract) does not apply.
pub async fn scenario_cache_015(backend: Arc<dyn ClusterCacheBackend>, time: TimeControl) {
    // The second poll's budget is what makes the duplicate check real, and it
    // must stay a wait rather than a drain of what is already queued: the
    // duplicate this scenario guards against is a backend echoing the mutation
    // locally *and* again off its notification channel, so the second copy
    // arrives a round-trip after the first. Both budgets are
    // backend-independent — a paused clock auto-advances to the deadline while
    // the runtime is idle, so `Virtual` pays no wall-clock for either (see
    // [`crate::watch_poll`]) — and both are shorter than the default because a
    // *passing* run always spends the second one in full.
    const FIRST_EVENT_BUDGET: Duration = Duration::from_millis(500);
    const DUPLICATE_BUDGET: Duration = Duration::from_millis(250);

    // Capability-gated on `features().watch` (see the doc): at-most-once delivery
    // is a watch property, so a watchless backend does not apply.
    if !backend.features().watch {
        return;
    }
    time.begin();
    let mut watch = backend.watch("k").await.expect("watch");
    backend
        .put(PutRequest {
            key: "k",
            value: b"v",
            ttl: Ttl::Indefinite,
        })
        .await
        .expect("put");

    // Annotated so the closure is higher-ranked over the borrow, and therefore
    // reusable across both polls.
    let changed = |event: &CacheEvent| matches!(event, CacheEvent::Changed { key } if key == "k");
    assert!(
        poll_watch(&mut watch)
            .upto(FIRST_EVENT_BUDGET)
            .until(changed)
            .await,
        "SC-CACHE-015: a single put must deliver a Changed event"
    );
    assert!(
        !poll_watch(&mut watch)
            .upto(DUPLICATE_BUDGET)
            .until(changed)
            .await,
        "SC-CACHE-015: a single put must deliver at most one Changed event"
    );
    time.end();
}

// TODO(SC-CACHE-016/017) [L4]: slow-subscriber `Lagged` and connection-loss
//   `Reset` are fault-injection scenarios delivered with the L4 harness, not the
//   in-process suite.

/// A helper plugins may use to assert a backend's self-declared consistency
/// before handing it to the consistency-sensitive default backends.
#[must_use]
pub fn is_linearizable(backend: &dyn ClusterCacheBackend) -> bool {
    matches!(backend.consistency(), CacheConsistency::Linearizable)
}
