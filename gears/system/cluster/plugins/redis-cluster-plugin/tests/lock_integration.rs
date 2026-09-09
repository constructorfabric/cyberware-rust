//! Layer 3 — lock integration scenarios (docs/TESTING.md §4.3), `RD-LOCK-001`
//! through `RD-LOCK-015`. `RD-LOCK-014` runs on the Sentinel fixture.
//!
//! These run against the **standalone** `RedisLockPlugin` unless a scenario is
//! specifically about the combined plugin or about wiring. That is the shape
//! `ClusterLockProvider::build_lock` takes in production (DESIGN.md §3.5), and it
//! is also the smaller thing to start: no cache, no watcher registry, one pool and
//! one subscriber.
//!
//! # What these hold that conformance does not
//!
//! `SC-LOCK-*` pins the contract — acquire, contend, time out, renew, release.
//! What it cannot see is *how* the lease is represented, and nearly every lock bug
//! worth catching lives there:
//!
//! - the lease is a key whose **value is the holder's token**, which is what makes
//!   `renew` and `release` fenceable (`RD-LOCK-006`, the bare-`DEL` regression);
//! - reclamation is **Redis expiry and nothing else** — there is no sweep in this
//!   plugin to stall (`RD-LOCK-004`);
//! - a blocked acquire wakes on a **publish**, not on its heartbeat, and the
//!   difference is two orders of magnitude (`RD-LOCK-003`);
//! - a held lock consumes **no connection** (`RD-LOCK-010`);
//! - `stop()` deliberately **leaves leases behind** to expire (`RD-LOCK-013`).

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests: a setup failure IS the test failure"
)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use cluster_sdk::{ClusterError, DistributedLockBackend};
use fred::interfaces::KeysInterface;
use redis_cluster_plugin::{RedisLockHandle, RedisLockPlugin, logs};
use serde_json::json;

/// A generous TTL for scenarios that are not about expiry, long enough that a
/// slow CI container cannot let a lease lapse mid-scenario and turn a contention
/// assertion into a spurious acquisition.
const LONG_TTL: Duration = Duration::from_secs(30);

/// The Redis key a lease is stored under — `<prefix>:l:<name>` (DESIGN.md §2.1).
///
/// Written out rather than read from `LockNames` for the same reason
/// `cache_integration.rs` spells out its entry key: a wire-format assertion that
/// derives the format from the code under test asserts nothing.
fn lease_key(prefix: &str, name: &str) -> String {
    format!("{prefix}:l:{name}")
}

/// Spawns a blocked `lock` on `name`, returning its outcome **and the instant it
/// acquired**.
///
/// Returning the acquisition instant rather than an elapsed duration is what makes
/// the three publish-driven-wake scenarios measure the right interval. The waiter
/// necessarily starts before the holder releases — it has to be blocked for a
/// release to wake it — so the time from *its* start includes however long the
/// test chose to wait first, which swamps the few milliseconds actually under
/// test. The interval that means anything is release → acquire, and only the
/// releasing side knows when the release happened.
fn spawn_waiter(
    lock: &Arc<dyn DistributedLockBackend>,
    name: &'static str,
) -> tokio::task::JoinHandle<(Result<cluster_sdk::lock::LockGuard, ClusterError>, Instant)> {
    let lock = Arc::clone(lock);
    tokio::spawn(async move {
        let guard = lock.lock(name, LONG_TTL, Duration::from_secs(10)).await;
        (guard, Instant::now())
    })
}

/// How many release→wake samples [`assert_woken_by_publish_repeatedly`] takes.
/// Odd, so the median is a single observed sample rather than an average of two,
/// and small enough that five ~200 ms setups stay cheap against the suite's ten
/// seconds.
const WAKE_SAMPLES: usize = 5;

/// Asserts a blocked acquire was woken by the release **publish** rather than by
/// the fallback heartbeat, over the **median** of [`WAKE_SAMPLES`] release→wake
/// cycles.
///
/// The margin is the assertion, not the bound. `HEARTBEAT` is 250 ms and the wake
/// delay is uniform over `[0, min(PTTL, 250 ms, remaining budget)]`, so a
/// heartbeat-driven wake averages ~125 ms and a publish-driven one lands in single
/// digits. 60 ms sits far enough below the fallback's mean that a pass cannot have
/// come from a lucky jitter draw, and far enough above a publish round trip that
/// the happy path clears it comfortably.
///
/// Why the median rather than one sample: a single wall-clock reading also rode
/// one scheduler stall, and a loaded CI box can delay the spawned waiter's wakeup
/// past 60 ms on a genuinely publish-driven wake — which then fails as exactly the
/// correctness bug this is meant to catch. The median needs a *majority* of the
/// samples to stall before it misreports, which a transient spike cannot do, while
/// a genuinely missed notification wakes near the ~125 ms heartbeat mean on
/// *every* sample and so is still caught. Median specifically, because the other
/// two order statistics move the wrong way here: min-of-N would let a missed
/// wake's occasional fast jitter draw pass, and max-of-N would turn a single stall
/// straight back into a failure.
///
/// A missed notification is the scenario the `PING` barrier of DESIGN.md §3.2
/// step 4 exists for.
///
/// `holder_lock` acquires and holds the name each cycle; `waiter_lock` is the
/// instance whose blocked `lock()` the release must wake — the same instance for
/// the local scenarios, and the *other* replica for `RD-LOCK-011`'s cross-instance
/// publish.
async fn assert_woken_by_publish_repeatedly(
    holder_lock: &Arc<dyn DistributedLockBackend>,
    waiter_lock: &Arc<dyn DistributedLockBackend>,
    name: &'static str,
) {
    let mut latencies = Vec::with_capacity(WAKE_SAMPLES);
    for sample in 0..WAKE_SAMPLES {
        let holder = holder_lock
            .try_lock(name, LONG_TTL)
            .await
            .unwrap_or_else(|error| panic!("sample {sample}: the holder acquires: {error:?}"));
        let waiter = spawn_waiter(waiter_lock, name);
        // Let the waiter reach its first failed `SET NX` and register its interest
        // before the release, so a wake can only be the publish (DESIGN.md §5.3).
        tokio::time::sleep(Duration::from_millis(200)).await;
        let released_at = Instant::now();
        holder.release().await.expect("the holder releases");
        let (acquired, acquired_at) = waiter.await.expect("the waiter task does not panic");
        let guard = acquired.expect("the waiter acquires once the holder releases");
        latencies.push(acquired_at.saturating_duration_since(released_at));
        guard.release().await.expect("the waiter's guard releases");
    }
    latencies.sort_unstable();
    #[expect(
        clippy::integer_division,
        reason = "the middle index of an odd-length sorted slice is exactly the median"
    )]
    let median = latencies[latencies.len() / 2];
    assert!(
        median < Duration::from_millis(60),
        "the wake must be publish-driven: a median wake at or near the 250 ms heartbeat means the \
         release notification was missed rather than merely slow. Samples (sorted): {latencies:?}"
    );
}

/// Starts a standalone lock plugin over a stock container.
///
/// Returns the container (which must outlive the test), the handle (which
/// **every** scenario must `stop()` — `RedisLockHandle` panics on drop without it
/// in a debug build, ADR-006), the lock backend, a raw client, the key prefix, and
/// the URL for scenarios that start a second instance.
async fn fixture(
    overrides: serde_json::Value,
) -> (
    testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>,
    RedisLockHandle,
    Arc<dyn DistributedLockBackend>,
    fred::clients::Client,
    String,
    String,
) {
    let (container, config) = common::start_redis_lock_only_with(overrides).await;
    let url = config.url.clone();
    let key_prefix = config.key_prefix.clone();
    let database = config.database;
    let handle = RedisLockPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the standalone lock plugin starts against the test container");
    let lock = handle.lock();
    let raw = common::raw_client_on(&url, database).await;
    (container, handle, lock, raw, key_prefix, url)
}

/// `RD-LOCK-001` — `try_lock` writes a real lease, a second attempt contends, and
/// `release` frees the name.
///
/// The second `try_lock` is issued **from the same instance**, deliberately.
/// Nothing in this plugin tracks locally-held names, so it is refused by the same
/// `SET NX` a foreign instance would hit — and a regression that added a local
/// short-circuit "optimisation" would make this instance's own second attempt
/// behave differently from everyone else's, which is the bug this pins.
#[tokio::test]
async fn rd_lock_001_try_lock_writes_a_lease_and_release_frees_it() {
    let (_container, handle, lock, raw, prefix, _url) = fixture(json!({})).await;
    let key = lease_key(&prefix, "res");

    let guard = lock
        .try_lock("res", LONG_TTL)
        .await
        .expect("try_lock on a free name succeeds");

    let token: String = raw.get(&key).await.expect("GET on the lease key succeeds");
    assert!(
        uuid::Uuid::parse_str(&token).is_ok(),
        "the lease value must be the holder token - a v4 UUID (DESIGN.md sec 5.1) - got {token:?}"
    );
    let pttl: i64 = raw.pttl(&key).await.expect("PTTL succeeds");
    // Compared in `u128`, the type `as_millis` returns, rather than casting it
    // down to the `i64` Redis reports: the cast is the only lossy step in the
    // comparison and it is avoidable.
    let remaining = u128::try_from(pttl).unwrap_or(0);
    assert!(
        pttl > 0 && remaining <= LONG_TTL.as_millis(),
        "the lease must carry the requested deadline, got {pttl}"
    );

    let contended = lock.try_lock("res", LONG_TTL).await;
    assert!(
        matches!(contended, Err(ClusterError::LockContended { .. })),
        "a second try_lock - even from this same instance - must contend, got {contended:?}"
    );

    guard.release().await.expect("release succeeds");
    let exists: i64 = raw.exists(&key).await.expect("EXISTS succeeds");
    assert_eq!(exists, 0, "release must remove the lease key");
    let reacquired = lock
        .try_lock("res", LONG_TTL)
        .await
        .expect("the name is acquirable again after release");
    reacquired.release().await.expect("release succeeds");

    handle.stop().await;
}

/// `RD-LOCK-002` — a blocked `lock` reports `LockTimeout`, not `Provider`, and
/// leaves nothing behind.
///
/// The distinction is the whole scenario (DESIGN.md §5.3): `LockTimeout` means
/// "someone else holds it" and `Provider` means "the backend is broken", and a
/// caller's retry policy should differ. A blocking loop that reported its last
/// transient error instead of classifying the outcome would look identical from
/// the outside until an operator tried to alert on it.
#[tokio::test]
async fn rd_lock_002_a_blocked_lock_times_out_and_leaves_nothing_behind() {
    let (_container, handle, lock, _raw, _prefix, _url) = fixture(json!({})).await;

    let holder = lock
        .try_lock("res", LONG_TTL)
        .await
        .expect("the holder acquires");

    let started = Instant::now();
    let blocked = lock.lock("res", LONG_TTL, Duration::from_millis(600)).await;
    let waited = started.elapsed();
    assert!(
        matches!(blocked, Err(ClusterError::LockTimeout { .. })),
        "a contended blocking acquire must report LockTimeout rather than a Provider error, got \
         {blocked:?}"
    );
    assert!(
        waited >= Duration::from_millis(500),
        "it must actually have waited its budget rather than failing fast, took {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(3),
        "and must not have overrun it, took {waited:?}"
    );

    holder.release().await.expect("release succeeds");
    let after = lock
        .try_lock("res", LONG_TTL)
        .await
        .expect("the timed-out attempt left no registration or lease behind");
    after.release().await.expect("release succeeds");

    handle.stop().await;
}

/// `RD-LOCK-003` — a blocked `lock` wakes on the release **publish**, far under
/// the 250 ms heartbeat.
///
/// The sharpest assertion in this file, and it is sharp because of the margin
/// rather than the bound: a wake measured at roughly one heartbeat means the
/// notification was *missed* and the fallback poll picked it up, which is a
/// correctness bug wearing a latency costume. A wake in single-digit milliseconds
/// can only have come from the publish.
///
/// It is also why DESIGN.md §3.2 step 4 awaits the initial subscribe before
/// `build_and_start` returns — and why that await is followed by a `PING`
/// (DESIGN.md §3.2 step 4): `fred` resolves a `psubscribe` when the
/// command reaches the connection, not when the server has processed it, so a
/// release landing in that window would otherwise reach no subscriber.
#[tokio::test]
async fn rd_lock_003_a_blocked_lock_wakes_on_the_release_publish() {
    let (_container, handle, lock, _raw, _prefix, _url) = fixture(json!({})).await;

    // Each cycle registers the waiter's interest before the release (the helper's
    // 200 ms settle), so a wake can only be the publish (DESIGN.md §5.3), and the
    // median over the cycles is what one scheduler stall cannot flip.
    assert_woken_by_publish_repeatedly(&lock, &lock, "res").await;

    handle.stop().await;
}

/// `RD-LOCK-004` — an abandoned lease is reclaimed by Redis expiry alone, and the
/// abandoning holder learns it lost the lock.
///
/// The clearest statement of what a native TTL buys (DESIGN.md §5.1): holder A
/// never renews and never releases, and B acquires purely because the key
/// expired. There is no reaper in this plugin to be stalled, no sweep interval to
/// tune, and no lock table to grow — which is exactly the class of failure the
/// Postgres plugin has to test around.
///
/// A's follow-up `renew` reporting `LockExpired` is the other half: a holder whose
/// lease lapsed must find out, or it would keep running a critical section two
/// processes are now inside.
#[tokio::test]
async fn rd_lock_004_an_expired_lease_is_reclaimed_with_no_reaper() {
    let (_container, handle, lock, _raw, _prefix, _url) = fixture(json!({})).await;

    let abandoned = lock
        .try_lock("res", Duration::from_millis(500))
        .await
        .expect("A acquires a short lease");

    let reclaimed = common::wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        async || {
            match lock.try_lock("res", LONG_TTL).await {
                Ok(guard) => {
                    // Leak deliberately: releasing here would free the name before the
                    // assertions below, and the handle's `stop()` leaves leases to
                    // expire anyway.
                    std::mem::forget(guard);
                    true
                }
                Err(_still_held) => false,
            }
        },
    )
    .await;
    assert!(
        reclaimed,
        "B must acquire on Redis expiry alone - nothing in this plugin sweeps abandoned leases"
    );

    let renewed = abandoned.renew(LONG_TTL).await;
    assert!(
        matches!(renewed, Err(ClusterError::LockExpired { .. })),
        "A's renew after its lease lapsed must report LockExpired rather than silently \
         resurrecting a lock B now holds, got {renewed:?}"
    );

    handle.stop().await;
}

/// `RD-LOCK-005` — `renew` extends the lease past its original deadline.
///
/// Asserted through `PTTL` rather than only through the return value, because
/// "renew returned Ok" and "the server's deadline moved" are different claims and
/// only the second one keeps the lock held.
#[tokio::test]
async fn rd_lock_005_renew_extends_the_lease() {
    let (_container, handle, lock, raw, prefix, _url) = fixture(json!({})).await;
    let key = lease_key(&prefix, "res");

    let guard = lock
        .try_lock("res", Duration::from_secs(1))
        .await
        .expect("acquire with a short lease");
    let before: i64 = raw.pttl(&key).await.expect("PTTL succeeds");
    assert!(before <= 1_000, "the original deadline is one second");

    guard
        .renew(Duration::from_secs(30))
        .await
        .expect("renew succeeds");
    let after: i64 = raw.pttl(&key).await.expect("PTTL succeeds");
    assert!(
        after > 1_000,
        "PTTL must reflect the new deadline, got {after} (was {before})"
    );

    // Past the original deadline, the lock is still held.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let contended = lock.try_lock("res", LONG_TTL).await;
    assert!(
        matches!(contended, Err(ClusterError::LockContended { .. })),
        "the renewed lock must outlive its original deadline, got {contended:?}"
    );

    guard.release().await.expect("release succeeds");
    handle.stop().await;
}

/// `RD-LOCK-006` — `renew` and `release` are token-fenced.
///
/// **The one test that fails on the classic bare-`DEL`-on-release bug.** A's lease
/// lapses and B acquires the same name; A then calls `release`, which on a naive
/// implementation deletes whatever is under the key — B's lease — handing the lock
/// to a third party while B believes it holds it. Here `release` is a Lua script
/// that deletes only if the value is still A's token (DESIGN.md §5.2), so A's call
/// is a no-op and B's key survives.
///
/// The assertion is on **B's token still being under the key**, not merely on the
/// key existing: a `release` that deleted and a `try_lock` that raced back in
/// would leave a key there too.
#[tokio::test]
async fn rd_lock_006_renew_and_release_are_token_fenced() {
    let (_container, handle, lock, raw, prefix, _url) = fixture(json!({})).await;
    let key = lease_key(&prefix, "res");

    let stale = lock
        .try_lock("res", Duration::from_millis(400))
        .await
        .expect("A acquires a short lease");
    let lapsed = common::wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        async || {
            let exists: i64 = raw.exists(&key).await.unwrap_or(1);
            exists == 0
        },
    )
    .await;
    assert!(lapsed, "A's lease must lapse before B acquires");

    let fresh = lock
        .try_lock("res", LONG_TTL)
        .await
        .expect("B acquires the freed name");
    let b_token: String = raw.get(&key).await.expect("GET succeeds");

    let renewed = stale.renew(LONG_TTL).await;
    assert!(
        matches!(renewed, Err(ClusterError::LockExpired { .. })),
        "A's renew must report LockExpired rather than extending B's lease, got {renewed:?}"
    );

    stale
        .release()
        .await
        .expect("A's release is a no-op rather than an error - it held nothing to give back");
    let after: String = raw
        .get(&key)
        .await
        .expect("B's lease key must still be present after A's stale release");
    assert_eq!(
        after, b_token,
        "A's stale release must leave B's key intact. A bare DEL-on-release would have removed it, \
         handing the lock to whoever asked next while B was still inside its critical section"
    );

    fresh.release().await.expect("B's release succeeds");
    handle.stop().await;
}

/// `RD-LOCK-007` — 20 concurrent local acquirers, at most one holder.
///
/// Kept distinct from `RD-LOCK-011` even though both exercise `SET NX`: the claim
/// worth holding both halves to is that local and cross-instance contention are
/// arbitrated *identically*. A regression that added a local fast path — an
/// in-process held-names set consulted before the round trip — would pass
/// `RD-LOCK-011` and show up here first.
#[tokio::test]
async fn rd_lock_007_twenty_local_acquirers_yield_one_holder() {
    let (_container, handle, lock, _raw, _prefix, _url) = fixture(json!({ "pool_size": 8 })).await;

    let mut tasks = Vec::new();
    for _ in 0..20 {
        let lock = Arc::clone(&lock);
        tasks.push(tokio::spawn(
            async move { lock.try_lock("res", LONG_TTL).await },
        ));
    }

    let mut winners = Vec::new();
    let mut contended = 0_u32;
    for task in tasks {
        match task.await.expect("no acquirer task panics") {
            Ok(guard) => winners.push(guard),
            Err(ClusterError::LockContended { .. }) => contended += 1,
            Err(other) => panic!("a losing acquirer must report LockContended, got {other:?}"),
        }
    }
    assert_eq!(
        winners.len(),
        1,
        "exactly one of 20 acquirers may hold the lock"
    );
    assert_eq!(contended, 19, "the other 19 must all report LockContended");

    for guard in winners {
        guard.release().await.expect("release succeeds");
    }
    handle.stop().await;
}

/// `RD-LOCK-008` — end-to-end YAML routing: the lock on Redis, the cache on
/// another provider entirely.
///
/// This is the scenario that needs the `cluster/src/gear.rs` registration, and it is the
/// only thing that proves `provider: redis` under `lock` is reachable *through the
/// wiring* rather than merely callable off this plugin's own builder. It exercises
/// the SDK's "non-cache providers do not receive the cache backend" contract from
/// the operator's side: the profile binds `cache: { provider: standalone }`, so if
/// the lock provider consulted a cache at all it would be an in-process one, and
/// the lease key asserted below would not exist on the server.
#[tokio::test]
async fn rd_lock_008_end_to_end_yaml_routing_lock_redis_cache_standalone() {
    use cluster::{ClusterConfig, ClusterWiring, ProfileRegistry, ProviderRegistry};
    use cluster_sdk::lock::DistributedLockV1;
    use cluster_sdk::profile::ClusterProfile;
    use redis_cluster_plugin::RedisLockProvider;
    use standalone_cluster_plugin::StandaloneCacheProvider;
    use toolkit::client_hub::ClientHub;

    #[derive(Clone, Copy)]
    struct RoutingProfile;
    impl ClusterProfile for RoutingProfile {
        const NAME: &'static str = "redislockrouting";
    }

    let (_container, config) = common::start_redis_lock_only().await;
    let url = config.url.clone();
    let key_prefix = config.key_prefix.clone();

    // Operator config is normally YAML, but `ClusterConfig` is a plain
    // `serde::Deserialize` type — an equivalent JSON value travels the identical
    // `BackendBinding`/flattened-`options` path without a YAML dev-dependency.
    let mut profiles = serde_json::Map::new();
    profiles.insert(
        RoutingProfile::NAME.to_owned(),
        json!({
            "cache": { "provider": "standalone" },
            "lock": { "provider": "redis", "url": url, "pool_size": 4 },
        }),
    );
    let cluster_config: ClusterConfig = serde_json::from_value(json!({ "profiles": profiles }))
        .expect("the routing profile config parses");

    let providers = ProviderRegistry::new()
        .with_cache_provider(Arc::new(StandaloneCacheProvider))
        .with_lock_provider(Arc::new(RedisLockProvider));
    let hub = Arc::new(ClientHub::new());
    let (mut handle, bound) =
        ClusterWiring::from_config(Arc::clone(&hub), &cluster_config, &providers)
            .await
            .expect("the wiring must resolve lock: redis independently of cache: standalone");
    handle.publish(&Arc::new(ProfileRegistry::new()), bound);

    let lock = DistributedLockV1::resolver(&hub)
        .profile(RoutingProfile)
        .resolve()
        .await
        .expect("the lock facade resolves for the routing profile");
    let guard = lock
        .try_lock("res", LONG_TTL)
        .await
        .expect("try_lock succeeds through the resolved facade");

    // A separate connection must see the lease: that is what makes this a real
    // Redis lock rather than the in-process standalone one the cache is bound to.
    let raw = common::raw_client(&url).await;
    let token: String = raw
        .get(lease_key(&key_prefix, "res"))
        .await
        .expect("the lease key must exist on the server");
    assert!(
        uuid::Uuid::parse_str(&token).is_ok(),
        "the resolved facade must be backed by a real Redis lease, got {token:?}"
    );

    guard.release().await.expect("release succeeds");
    handle.stop().await;
}

/// `RD-LOCK-009` — the standalone lock needs no keyspace notifications.
///
/// Against a container started with `notify-keyspace-events` empty. The
/// distinction that makes this work (DESIGN.md §3.5, third row): a release is a
/// `PUBLISH` **this plugin issues itself** on a channel it owns, not a keyspace
/// notification the server emits — so the wake path is entirely independent of a
/// server-wide setting the operator may have no ability to change. Only the
/// *cache*'s `Expired` events need the flags, and a lock-only deployment has no
/// cache.
///
/// Asserted alongside: the server really does have no flags set, so the scenario
/// cannot pass because a previous test configured the container.
#[tokio::test]
async fn rd_lock_009_the_standalone_lock_needs_no_keyspace_notifications() {
    let (_container, config) = common::start_redis_lock_only_no_notifications().await;
    let url = config.url.clone();
    let handle = RedisLockPlugin::builder(config)
        .build_and_start()
        .await
        .expect("the standalone lock plugin starts against a server with no keyspace events");
    let lock = handle.lock();
    let raw = common::raw_client(&url).await;

    assert_eq!(
        common::keyspace_flags(&raw).await,
        "",
        "the fixture must genuinely have no keyspace notifications, or this scenario proves nothing"
    );

    // Every acquisition path behaves as it does on a fully-configured server.
    let holder = lock.try_lock("res", LONG_TTL).await.expect("try_lock");
    assert!(
        matches!(
            lock.try_lock("res", LONG_TTL).await,
            Err(ClusterError::LockContended { .. })
        ),
        "contention is arbitrated by SET NX, which no server setting affects"
    );
    holder.renew(LONG_TTL).await.expect("renew");
    holder
        .release()
        .await
        .expect("release the setup holder before the timed cycles");

    // Still publish-driven on a server with no keyspace notifications at all,
    // because a release is this plugin's own PUBLISH rather than anything the
    // server emits.
    assert_woken_by_publish_repeatedly(&lock, &lock, "res").await;

    handle.stop().await;
}

/// `RD-LOCK-010` — held locks consume no connections.
///
/// `pool_size: 2` with 12 locks held at once. On a lock implementation that pins a
/// connection per held lock — which is what a Postgres session-level advisory lock
/// does, and why that plugin had to move off it — the third acquisition would
/// block forever. Here a lease is a key with a TTL (DESIGN.md §3.3), so the pool
/// is sized for pipelining rather than for concurrency.
///
/// The `renew` at the end is the part that would catch a *leaked* connection
/// rather than a pinned one: if each acquisition had quietly retained a pooled
/// connection, the pool would be exhausted and this call would time out.
#[tokio::test]
async fn rd_lock_010_held_locks_consume_no_connections() {
    let (_container, handle, lock, raw, prefix, _url) = fixture(json!({ "pool_size": 2 })).await;

    let mut guards = Vec::new();
    for index in 0..12 {
        guards.push(
            lock.try_lock(&format!("res-{index}"), LONG_TTL)
                .await
                .unwrap_or_else(|err| {
                    panic!(
                        "holding 12 locks on a pool of 2 must succeed; lock {index} failed: {err:?}"
                    )
                }),
        );
    }

    for index in 0..12 {
        let exists: i64 = raw
            .exists(lease_key(&prefix, &format!("res-{index}")))
            .await
            .expect("EXISTS succeeds");
        assert_eq!(exists, 1, "every held lease must exist on the server");
    }

    guards[0]
        .renew(LONG_TTL)
        .await
        .expect("a renew must still get a pool connection while 12 locks are held");

    for guard in guards {
        guard.release().await.expect("release succeeds");
    }
    handle.stop().await;
}

/// `RD-LOCK-011` — two independent instances cannot hold the same lock.
///
/// The cross-replica guarantee the whole primitive rests on, and the one property
/// no single-instance test can establish: two plugin instances with separate pools
/// and separate subscribers, arbitrated only by the server's `SET NX`.
///
/// B waking on A's release is the second half, and it is a *cross-process* publish
/// rather than the in-process waiter registry — B's fan-out receives a message A's
/// pool sent, which is the only reason a blocked acquire in one replica is fast
/// when the holder is in another.
#[tokio::test]
async fn rd_lock_011_two_instances_cannot_hold_the_same_lock() {
    let (_container, handle_a, lock_a, raw, prefix, url) = fixture(json!({})).await;
    let key = lease_key(&prefix, "res");

    // The same `key_prefix` and database, a different instance — two replicas of
    // one deployment, which is the arrangement under test.
    let config_b = common::lock_config_json(&url, json!({ "key_prefix": prefix.clone() }));
    let handle_b = RedisLockPlugin::builder(config_b)
        .build_and_start()
        .await
        .expect("a second independent instance starts");
    let lock_b = handle_b.lock();

    let held_by_a = lock_a.try_lock("res", LONG_TTL).await.expect("A acquires");
    let a_token: String = raw.get(&key).await.expect("GET succeeds");
    assert!(
        matches!(
            lock_b.try_lock("res", LONG_TTL).await,
            Err(ClusterError::LockContended { .. })
        ),
        "B must contend on a name A holds"
    );
    let keys_present: i64 = raw.exists(&key).await.expect("EXISTS succeeds");
    assert_eq!(keys_present, 1, "exactly one lease key exists for the name");

    // One real cross-instance cycle for the token assertion: B must acquire under
    // its *own* token once A releases, not inherit A's.
    let waiter = spawn_waiter(&lock_b, "res");
    tokio::time::sleep(Duration::from_millis(200)).await;
    held_by_a.release().await.expect("A releases");

    let (acquired, _acquired_at) = waiter.await.expect("B's waiter task does not panic");
    let held_by_b = acquired.expect("B acquires as soon as A releases");
    let b_token: String = raw.get(&key).await.expect("GET succeeds");
    assert_ne!(
        a_token, b_token,
        "B must hold under its own token, not inherit A's"
    );
    held_by_b.release().await.expect("B releases");

    // B's wake rides A's *cross-instance* publish: B's own subscriber received a
    // message A's pool sent, which is the only reason a blocked acquire in one
    // replica is fast when the holder is in another. Measured as a median over
    // several cross-instance cycles so one scheduler stall cannot fail it.
    assert_woken_by_publish_repeatedly(&lock_a, &lock_b, "res").await;

    handle_b.stop().await;
    handle_a.stop().await;
}

/// `RD-LOCK-012` — after `stop()`, an acquisition answers `Shutdown` immediately.
///
/// Immediately is the assertion. Without the pre-work check, a blocking `lock`
/// against a torn-down backend would retry for its whole 30 s budget and then
/// report `LockTimeout` — which tells a caller "someone else holds it" when the
/// truth is "this backend is gone". Those need different handling, and the only
/// way a caller can tell them apart is if the backend says so.
///
/// `try_lock` is asserted alongside because both take the same check, and a
/// regression that guarded only the blocking path would leave the cheaper one
/// reporting a connection error from a closed pool.
#[tokio::test]
async fn rd_lock_012_acquiring_after_stop_answers_shutdown_at_once() {
    let (_container, handle, lock, _raw, _prefix, _url) = fixture(json!({})).await;
    handle.stop().await;

    let started = Instant::now();
    let blocked = lock.lock("res", LONG_TTL, Duration::from_secs(30)).await;
    let elapsed = started.elapsed();
    assert!(
        matches!(blocked, Err(ClusterError::Shutdown)),
        "a blocking lock after stop() must report Shutdown, not LockTimeout, got {blocked:?}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "and must report it at once rather than spending the whole 30 s budget, took {elapsed:?}"
    );

    let immediate = lock.try_lock("res", LONG_TTL).await;
    assert!(
        matches!(immediate, Err(ClusterError::Shutdown)),
        "try_lock takes the same pre-work check, got {immediate:?}"
    );
}

/// `RD-LOCK-013` — `stop()` leaves held leases behind, and they expire on their
/// own deadlines.
///
/// **Deliberately asserts that cleanup does *not* happen.**
/// `cpt-cf-clst-fr-shutdown-ttl-cleanup` forbids best-effort remote cleanup on
/// shutdown, and a lease needs none: it reaps itself. So there is no drain step to
/// time out, no statement that can half-succeed, and no partial-cleanup failure
/// mode for an operator to alert on.
///
/// A future "tidy up on stop" change would fail here, which is the point — it
/// would look like an improvement and would introduce a shutdown path that can
/// fail against an unresponsive server.
#[tokio::test]
async fn rd_lock_013_stop_leaves_held_leases_to_expire() {
    let (_container, handle, lock, raw, prefix, _url) = fixture(json!({})).await;

    let mut guards = Vec::new();
    for index in 0..3 {
        guards.push(
            lock.try_lock(&format!("res-{index}"), Duration::from_secs(4))
                .await
                .expect("acquire"),
        );
    }
    // Dropped rather than released: a supervisor shutting the gear down does not
    // hand every guard back first, which is the situation this scenario is about.
    drop(guards);
    handle.stop().await;

    for index in 0..3 {
        let exists: i64 = raw
            .exists(lease_key(&prefix, &format!("res-{index}")))
            .await
            .expect("EXISTS succeeds");
        assert_eq!(
            exists, 1,
            "lease res-{index} must still be present after stop() - shutdown does no remote \
             cleanup (cpt-cf-clst-fr-shutdown-ttl-cleanup)"
        );
    }

    let all_expired = common::wait_until(
        Duration::from_secs(10),
        Duration::from_millis(200),
        async || {
            let mut remaining = 0_i64;
            for index in 0..3 {
                remaining += raw
                    .exists::<i64, _>(lease_key(&prefix, &format!("res-{index}")))
                    .await
                    .unwrap_or(1);
            }
            remaining == 0
        },
    )
    .await;
    assert!(
        all_expired,
        "and each must then lapse on its own deadline - the reason no cleanup is needed"
    );
}

/// `RD-LOCK-015` — the **standalone** lock plugin observes an evicted lease.
///
/// The half of DESIGN.md §3.7 that a cache-scoped pattern cannot reach at all
/// rather than merely narrowly. Without its own keyspace subscription this
/// plugin — the shape `ClusterLockProvider::build_lock` produces in production —
/// has every lease it holds evicted out from under it and reports nothing.
///
/// It matters most precisely here. A lock-only deployment is the one likeliest to
/// be pointed at a *shared* Redis, since it has no cache whose working set would
/// argue for its own instance, and a shared cache instance under `allkeys-lru` is
/// the documented misconfiguration §3.7 exists to report.
///
/// The pressure comes from a raw client rather than from the thing under test:
/// this plugin owns no cache to write filler through, which is the same reason
/// the fixture hands back a URL.
///
/// What is *not* asserted is any change in behaviour. The lock keeps working
/// through the eviction — the acquisition loop treats the `SET NX` as the source
/// of truth throughout — and the plugin still needs no `expired` flag. The whole
/// of the response is the report.
#[tokio::test]
async fn rd_lock_015_the_standalone_plugin_observes_an_evicted_lease() {
    let (_guard, log) = common::scoped_capture(tracing::Level::WARN);
    let (meter, metrics) = common::in_memory_meter();
    let (_container, config, url) = common::start_redis_evicting_lock_only().await;
    let database = config.database;
    let handle = RedisLockPlugin::builder(config)
        .__with_meter(meter)
        .build_and_start()
        .await
        .expect("the standalone lock plugin starts against a memory-capped container");

    // Acquired first and never renewed, so it is the oldest untouched key when
    // `allkeys-lru` starts choosing. The TTL is far longer than the scenario, so
    // an expiry cannot be mistaken for the eviction under test.
    let _lease = handle
        .lock()
        .try_lock("evictable", Duration::from_mins(10))
        .await
        .expect("the lease is acquired on an empty container");

    let raw = common::raw_client_on(&url, database).await;
    let filler = vec![b'f'; 16 * 1024];
    let mut observed = false;
    for index in 0..1_200 {
        let _: () = raw
            .set(
                format!("filler:{index}"),
                filler.as_slice(),
                None,
                None,
                false,
            )
            .await
            .expect("filler writes succeed under allkeys-lru");
        if common::count_occurrences(&log, "primitive=\"lock\"") >= 1 {
            observed = true;
            break;
        }
    }

    assert!(
        observed,
        "the standalone plugin must report an evicted lease. It subscribed no keyspace pattern \
         at all before this landed, so a lock-only deployment on a shared evicting Redis was \
         silent about the one failure it most needed to report. Captured: {}",
        common::captured(&log)
    );
    assert!(
        common::count_occurrences(&log, logs::EVICTION_OBSERVED) >= 1,
        "under the same contracted event name the combined plugin uses"
    );
    assert!(
        metrics.counter("cluster_redis_evictions_observed_total") >= 1,
        "and counted, so an alert can fire on a lock-only deployment too"
    );

    handle.stop().await;
}

/// `RD-LOCK-014` — `WAIT` is applied when configured, a short count surfaces, and
/// the declaration is unchanged by either.
///
/// `wait.rs`'s short-count arm is the whole reason `WAIT` is not fire-and-forget:
/// an operator who opted into "this write is on a replica before I proceed" must
/// not silently not receive it. Reaching that arm needs a real replica that can
/// really stop acknowledging, which is why this waits on the Sentinel fixture.
///
/// The replica is **stopped** rather than partitioned. Partitioning is fault
/// injection and stays deferred (§8); what this arm is about is the *short count*,
/// and `WAIT` returning fewer acks than asked for is the same observable either
/// way — the plugin cannot tell a gone replica from an unreachable one, and is not
/// supposed to.
///
/// The third assertion is the one most easily lost: `WAIT` narrows a **window**,
/// not a guarantee. It does not make the topology durable, so it must not move
/// `consistency()` or `features().linearizable` (ADR-009, DESIGN.md §3.6). A
/// plugin that upgraded its declaration because `WAIT` was configured would hand
/// a consumer exactly the false assurance §3.6 exists to refuse.
#[tokio::test]
async fn rd_lock_014_wait_is_applied_and_a_short_count_surfaces() {
    let (container, config, primary, replica) = common::start_redis_sentinel().await;
    let lock_config = common::lock_config_json(
        &config.url,
        json!({ "wait_replicas": 1, "wait_timeout_ms": 500 }),
    );
    let handle = RedisLockPlugin::builder(lock_config)
        .build_and_start()
        .await
        .expect("the standalone lock plugin starts against a sentinel-managed primary");
    let lock = handle.lock();

    assert!(
        !lock.features().linearizable,
        "WAIT narrows the window in which a failover can lose an acknowledged write; it does not \
         close it. The declaration tracks the topology, and configuring WAIT must not upgrade it \
         (ADR-009, DESIGN.md sec 3.6)"
    );

    // With the replica online, `WAIT 1 500` is satisfied and the acquire is
    // ordinary. This half is what makes the failure half meaningful: without it,
    // a plugin that failed every acquire would also "pass" below.
    let guard = lock
        .try_lock("waited", LONG_TTL)
        .await
        .expect("an acquire succeeds while the replica is acknowledging");
    guard.release().await.expect("release succeeds");

    // Take the replica away. `SHUTDOWN NOSAVE` stops the process; the primary and
    // the sentinel are separate processes in the same container and keep running.
    let _ = common::exec_in(
        &container,
        &[
            "redis-cli",
            "-p",
            &replica.to_string(),
            "shutdown",
            "nosave",
        ],
    )
    .await;
    let detached = common::wait_until(
        Duration::from_secs(15),
        Duration::from_millis(200),
        async || {
            !common::exec_in(
                &container,
                &[
                    "redis-cli",
                    "-p",
                    &primary.to_string(),
                    "info",
                    "replication",
                ],
            )
            .await
            .contains("state=online")
        },
    )
    .await;
    assert!(
        detached,
        "the fixture's replica must actually detach, or the short-count arm is never reached"
    );

    let short = lock.try_lock("waited-again", LONG_TTL).await;
    assert!(
        matches!(
            short,
            Err(ClusterError::Provider {
                kind: cluster_sdk::ProviderErrorKind::ResourceExhausted,
                ..
            })
        ),
        "an acquire whose WAIT is not satisfied must surface ResourceExhausted rather than \
         reporting success: the lease is on the primary but not replicated, so a failover now \
         could hand the same lock to a second holder. Got {short:?}"
    );

    handle.stop().await;
}
