use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use std::time::Duration;

use async_trait::async_trait;
use cluster_sdk::{
    CacheCapability, CacheWatchEvent, ClusterCacheV1, ClusterError, ClusterProfile,
    DistributedLockBackend, DistributedLockV1, ElectionConfig, LeaderElectionBackend,
    LeaderElectionFeatures, LeaderElectionV1, LeaderStatus, LeaderWatch, LeaderWatchEvent,
    LockFeatures, LockGuard,
};
use standalone_cluster_plugin::StandaloneClusterPlugin;

use crate::defaults::test_cache::MemoryCache;
use crate::defaults::{CasBasedDistributedLockBackend, CasBasedLeaderElectionBackend};
use toolkit::client_hub::ClientHub;
use tracing_test::traced_test;

use super::{ClusterWiring, ProfileBackends};

#[derive(Clone, Copy)]
struct EventBroker;
impl ClusterProfile for EventBroker {
    const NAME: &'static str = "event-broker";
}

#[tokio::test]
async fn omit_default_registers_all_three_then_stop_unbinds() {
    let hub = Arc::new(ClientHub::new());
    let plugin = StandaloneClusterPlugin::builder()
        .build_and_start()
        .expect("plugin starts");
    let cache = plugin.cache();

    // Bind only the cache; the wiring auto-fills the other three with SDK defaults.
    let handle = ClusterWiring::builder(Arc::clone(&hub))
        .profile(EventBroker, ProfileBackends::new(cache))
        .on_stop(move || async move { plugin.stop().await })
        .build_and_start()
        .expect("wiring starts");

    assert!(
        ClusterCacheV1::resolver(&hub)
            .profile(EventBroker)
            .require(CacheCapability::Linearizable)
            .resolve()
            .await
            .is_ok(),
        "the bound linearizable cache resolves"
    );
    assert!(
        LeaderElectionV1::resolver(&hub)
            .profile(EventBroker)
            .resolve()
            .await
            .is_ok(),
        "omit-default leader election resolves"
    );
    assert!(
        DistributedLockV1::resolver(&hub)
            .profile(EventBroker)
            .resolve()
            .await
            .is_ok(),
        "omit-default lock resolves"
    );

    handle.stop().await;

    // Deregistration leaves the profile unbound.
    assert!(matches!(
        ClusterCacheV1::resolver(&hub)
            .profile(EventBroker)
            .resolve()
            .await,
        Err(ClusterError::ProfileNotBound { .. })
    ));
    assert!(matches!(
        LeaderElectionV1::resolver(&hub)
            .profile(EventBroker)
            .resolve()
            .await,
        Err(ClusterError::ProfileNotBound { .. })
    ));
}

#[tokio::test]
async fn stop_revokes_an_active_leader_before_shutdown_completes() {
    let hub = Arc::new(ClientHub::new());
    let plugin = StandaloneClusterPlugin::builder()
        .build_and_start()
        .expect("plugin starts");
    let cache = plugin.cache();

    let handle = ClusterWiring::builder(Arc::clone(&hub))
        .profile(EventBroker, ProfileBackends::new(cache))
        .on_stop(move || async move { plugin.stop().await })
        .build_and_start()
        .expect("wiring starts");

    // A consumer wins the omit-default (CAS-based) election.
    let leader = LeaderElectionV1::resolver(&hub)
        .profile(EventBroker)
        .resolve()
        .await
        .expect("leader election resolves");
    let mut watch = leader.elect("primary").await.expect("election joins");
    assert!(matches!(
        watch.changed().await,
        LeaderWatchEvent::Status(LeaderStatus::Leader)
    ));

    // Graceful shutdown must revoke leadership before it completes.
    handle.stop().await;

    // The former leader observes loss, then a terminal shutdown close, and its
    // synchronous snapshot no longer claims leadership.
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

#[tokio::test]
async fn stop_revokes_active_lock_and_cache_watches_before_shutdown_completes() {
    let hub = Arc::new(ClientHub::new());
    let plugin = StandaloneClusterPlugin::builder()
        .build_and_start()
        .expect("plugin starts");
    let cache_backend = plugin.cache();

    let handle = ClusterWiring::builder(Arc::clone(&hub))
        .profile(EventBroker, ProfileBackends::new(plugin.cache()))
        .on_stop(move || async move { plugin.stop().await })
        .build_and_start()
        .expect("wiring starts");

    // A blocking lock waiter that must keep waiting (the lock is held).
    let lock = DistributedLockV1::resolver(&hub)
        .profile(EventBroker)
        .resolve()
        .await
        .expect("lock resolves");
    let _held = lock
        .try_lock("ledger", Duration::from_secs(100))
        .await
        .expect("first holder acquires");
    let lock_waiter = lock.clone();
    let waiter = tokio::spawn(async move {
        lock_waiter
            .lock("ledger", Duration::from_secs(100), Duration::from_secs(100))
            .await
    });

    // An active cache watch.
    let mut cache_watch = cache_backend
        .watch("k")
        .await
        .expect("cache watch establishes");

    // Let the lock waiter and translator tasks reach their wait points.
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }

    // Graceful shutdown must revoke all in-flight coordination before completing.
    handle.stop().await;

    // The in-flight lock waiter resolves to Shutdown (not LockTimeout).
    let joined = waiter.await.expect("waiter task joins");
    assert!(
        matches!(joined, Err(ClusterError::Shutdown)),
        "an in-flight lock waiter must observe Shutdown on stop; got {joined:?}"
    );
    // The cache watch observes a terminal Closed(Shutdown) via the plugin stop hook.
    assert!(matches!(
        cache_watch.recv().await,
        Some(CacheWatchEvent::Closed(ClusterError::Shutdown))
    ));
}

// ---- Call-counting wrappers used only to prove an explicitly-bound backend
// (not the SDK default auto-filled by omit-default) is the instance that
// actually receives calls. Each wrapper delegates to a real backend and bumps
// a shared counter first, so a counter of zero after a call through the
// facade means the wrapped instance was NOT the one registered.

struct MarkerLeaderElectionBackend {
    inner: Arc<CasBasedLeaderElectionBackend>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LeaderElectionBackend for MarkerLeaderElectionBackend {
    fn features(&self) -> LeaderElectionFeatures {
        self.inner.features()
    }

    async fn elect(&self, name: &str) -> Result<LeaderWatch, ClusterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.elect(name).await
    }

    async fn elect_with_config(
        &self,
        name: &str,
        config: ElectionConfig,
    ) -> Result<LeaderWatch, ClusterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.elect_with_config(name, config).await
    }
}

struct MarkerLockBackend {
    inner: Arc<CasBasedDistributedLockBackend>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl DistributedLockBackend for MarkerLockBackend {
    fn features(&self) -> LockFeatures {
        self.inner.features()
    }

    async fn try_lock(&self, name: &str, ttl: Duration) -> Result<LockGuard, ClusterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.try_lock(name, ttl).await
    }

    async fn lock(
        &self,
        name: &str,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<LockGuard, ClusterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.lock(name, ttl, timeout).await
    }
}

/// Proves the explicitly-bound backends are the instances actually registered
/// and invoked — not the SDK defaults `build_and_start` would otherwise
/// auto-fill. Each primitive is bound to a marker that delegates to a real
/// default-flavored backend but increments its own call counter first; a
/// resolve-and-invoke through the public facade that leaves a counter at zero
/// would mean the explicit binding was silently dropped in favor of a default.
#[tokio::test]
async fn explicit_backends_override_defaults() {
    let hub = Arc::new(ClientHub::new());
    let plugin = StandaloneClusterPlugin::builder()
        .build_and_start()
        .expect("plugin starts");

    let cache = plugin.cache();
    let leader_calls = Arc::new(AtomicUsize::new(0));
    let lock_calls = Arc::new(AtomicUsize::new(0));

    let leader = Arc::new(MarkerLeaderElectionBackend {
        inner: Arc::new(
            CasBasedLeaderElectionBackend::new(Arc::clone(&cache))
                .expect("leader backend over linearizable cache"),
        ),
        calls: Arc::clone(&leader_calls),
    });
    let lock = Arc::new(MarkerLockBackend {
        inner: Arc::new(
            CasBasedDistributedLockBackend::new(Arc::clone(&cache))
                .expect("lock backend over linearizable cache"),
        ),
        calls: Arc::clone(&lock_calls),
    });
    let backends = ProfileBackends::new(cache)
        .with_leader_election(leader)
        .with_lock(lock);

    let handle = ClusterWiring::builder(Arc::clone(&hub))
        .profile(EventBroker, backends)
        .on_stop(move || async move { plugin.stop().await })
        .build_and_start()
        .expect("wiring starts");

    LeaderElectionV1::resolver(&hub)
        .profile(EventBroker)
        .resolve()
        .await
        .expect("leader election resolves")
        .elect("primary")
        .await
        .expect("election joins");
    assert_eq!(
        leader_calls.load(Ordering::SeqCst),
        1,
        "the explicitly-bound leader-election backend must receive the call, not an SDK default"
    );

    DistributedLockV1::resolver(&hub)
        .profile(EventBroker)
        .resolve()
        .await
        .expect("lock resolves")
        .try_lock("ledger", Duration::from_secs(30))
        .await
        .expect("lock acquires");
    assert_eq!(
        lock_calls.load(Ordering::SeqCst),
        1,
        "the explicitly-bound lock backend must receive the call, not an SDK default"
    );

    handle.stop().await;
}

// `ClusterHandle` Drop guard (ADR-006 §Confirmation)

/// A graceful `stop()` must disarm the Drop guard: the handle drops at end of
/// scope without panicking. Reaching the end of the test is the assertion.
#[tokio::test]
async fn stop_disarms_the_drop_guard() {
    let hub = Arc::new(ClientHub::new());
    let handle = ClusterWiring::builder(hub)
        .build_and_start()
        .expect("empty wiring starts");
    handle.stop().await;
    // `handle` drops here after a clean stop(); no panic, no warning.
}

/// Dropping a handle without `stop()` is a programming error that leaks plugin
/// background tasks — in debug builds the Drop guard turns it into a loud panic
/// rather than a silent leak.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "dropped without stop()")]
fn drop_without_stop_panics_in_debug() {
    let hub = Arc::new(ClientHub::new());
    // Even an empty wiring yields a handle that must be `stop()`ped.
    let handle = ClusterWiring::builder(hub)
        .build_and_start()
        .expect("empty wiring starts");
    drop(handle); // no stop() — the Drop guard fires
}

/// If a handle is dropped *while a panic is already unwinding*, the Drop guard
/// must warn instead of panicking again — a second panic during unwind aborts
/// the process. Catching the original panic here proves the process survived
/// (a double-panic abort is uncatchable), and the warning confirms the
/// `thread::panicking()` branch ran instead of the debug panic.
#[traced_test]
#[test]
fn drop_during_panic_warns_instead_of_double_panicking() {
    let hub = Arc::new(ClientHub::new());
    let handle = ClusterWiring::builder(hub)
        .build_and_start()
        .expect("empty wiring starts");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _doomed = handle; // dropped as the stack unwinds
        panic!("simulated consumer panic");
    }));

    assert!(
        result.is_err(),
        "the original panic must propagate - the process must not abort"
    );
    assert!(
        logs_contain("dropped during panic unwind"),
        "the Drop guard must warn (not panic) during unwind"
    );
}

/// A second typed profile, so two profiles can be bound to one cache instance.
#[derive(Clone, Copy)]
struct Scheduler;
impl ClusterProfile for Scheduler {
    const NAME: &'static str = "scheduler";
}

/// DESIGN 5.3: the point of recording instance identity is that sharing is
/// observable. Two profiles bound to the *same* cache `Arc` are served by one
/// instance - one store, one sweeper, one stop hook - and must report one id,
/// while each still gets its own SDK-default lock and election layered over it.
///
/// This is also the builder path's descriptor case: no operator config exists
/// here, so provider identity falls back to the concrete backend type, which is
/// the identity `CapabilityNotMet` already carries.
#[tokio::test]
async fn two_profiles_on_one_cache_report_one_cache_instance() {
    let hub = Arc::new(ClientHub::new());
    let plugin = StandaloneClusterPlugin::builder()
        .build_and_start()
        .expect("plugin starts");
    let cache = plugin.cache();

    let (handle, bound) = ClusterWiring::builder(Arc::clone(&hub))
        .profile(EventBroker, ProfileBackends::new(Arc::clone(&cache)))
        .profile(Scheduler, ProfileBackends::new(cache))
        .on_stop(move || async move { plugin.stop().await })
        .build_and_start_bound()
        .expect("wiring starts");

    assert_eq!(bound.len(), 2);
    let broker = &bound[0];
    let scheduler = &bound[1];
    assert_eq!(broker.name, "event-broker");
    assert_eq!(scheduler.name, "scheduler");

    assert_eq!(
        broker.instances.cache, scheduler.instances.cache,
        "one cache Arc bound to two profiles is one instance"
    );
    assert_ne!(
        broker.instances.lock, scheduler.instances.lock,
        "each profile gets its own SDK-default lock over that shared cache"
    );
    assert_ne!(
        broker.instances.leader_election, scheduler.instances.leader_election,
        "and its own SDK-default election"
    );

    // The builder path has no configured provider name, so the descriptor
    // reports the concrete backend type instead.
    assert!(
        broker
            .descriptor()
            .cache
            .provider
            .contains("StandaloneCache"),
        "expected the concrete cache backend type, got: {}",
        broker.descriptor().cache.provider
    );
    assert_eq!(
        broker.descriptor().cache.provider,
        scheduler.descriptor().cache.provider,
        "both profiles describe the same cache"
    );

    handle.stop().await;
}

// ---------------------------------------------------------------------------
// The programmatic weak-consistency opt-in
// (`ProfileBackends::allow_weak_leader_election` / `allow_weak_lock`).
//
// The config-driven half of this — the reserved `provider: default` binding that
// reaches these builders from operator YAML — is tested in `config_tests.rs`
// alongside the rest of the `from_config` path. These cover the builder API a
// host using `ClusterWiring::builder()` calls directly, which is a separate
// public entry point rather than a wrapper over the YAML one.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_weak_cache_profile_fails_without_the_builder_opt_ins() {
    let hub = Arc::new(ClientHub::new());

    let result = ClusterWiring::builder(hub)
        .profile(
            EventBroker,
            ProfileBackends::new(MemoryCache::eventually_consistent()),
        )
        .build_and_start();

    assert!(
        matches!(result, Err(ClusterError::InvalidConfig { .. })),
        "the CAS defaults' consistency guard must reject a weak cache by default"
    );
}

#[tokio::test]
async fn the_leader_opt_in_alone_still_fails_on_the_lock() {
    // Both CAS defaults share one guard, and the leader is resolved first, so a
    // single-flag implementation looks correct until the lock is reached. The
    // config-level counterpart is
    // `opting_in_on_leader_election_alone_still_fails_on_the_lock`.
    let hub = Arc::new(ClientHub::new());

    let result = ClusterWiring::builder(hub)
        .profile(
            EventBroker,
            ProfileBackends::new(MemoryCache::eventually_consistent()).allow_weak_leader_election(),
        )
        .build_and_start();

    let Err(ClusterError::InvalidConfig { reason }) = result else {
        panic!("the lock default's guard must still bite");
    };
    assert!(
        reason.contains("CasBasedDistributedLockBackend"),
        "the surviving failure must be the lock guard, got: {reason}"
    );
}

#[tokio::test]
async fn both_builder_opt_ins_start_a_weak_cache_profile() {
    let hub = Arc::new(ClientHub::new());

    let handle = ClusterWiring::builder(Arc::clone(&hub))
        .profile(
            EventBroker,
            ProfileBackends::new(MemoryCache::eventually_consistent())
                .allow_weak_leader_election()
                .allow_weak_lock(),
        )
        .build_and_start()
        .expect("both opt-ins present, so the profile must start");

    // The auto-filled defaults track the cache's declaration rather than claiming
    // a guarantee the opt-in cannot confer: waiving the *constructor guard* does
    // not make the backend linearizable.
    let leader = LeaderElectionV1::resolver(&hub)
        .profile(EventBroker)
        .resolve()
        .await
        .expect("the weak leader default resolves");
    assert!(
        !leader.features().linearizable,
        "the leader default must keep reporting linearizable: false over a weak cache"
    );
    let lock = DistributedLockV1::resolver(&hub)
        .profile(EventBroker)
        .resolve()
        .await
        .expect("the weak lock default resolves");
    assert!(
        !lock.features().linearizable,
        "the lock default must keep reporting linearizable: false over a weak cache"
    );

    handle.stop().await;
}

/// The hub stays all-or-nothing: a later profile failing registration rolls back
/// every profile registered before it (DESIGN §3.7). Returning the bound-profile
/// set does not change that — the set is built first, but nothing is published
/// from it, so a failure discards the whole batch.
#[tokio::test]
async fn a_failed_registration_rolls_back_the_profiles_before_it() {
    let hub = Arc::new(ClientHub::new());
    let plugin = StandaloneClusterPlugin::builder()
        .build_and_start()
        .expect("plugin starts");
    let cache = plugin.cache();

    // The second profile's name violates the cluster name rule, which
    // registration rejects — after the first profile has already registered.
    let result = ClusterWiring::builder(Arc::clone(&hub))
        .profile(EventBroker, ProfileBackends::new(Arc::clone(&cache)))
        .profile_named("not a valid name", ProfileBackends::new(cache))
        .build_and_start();

    assert!(
        matches!(result, Err(ClusterError::InvalidName { .. })),
        "an invalid profile name must fail registration"
    );
    // A failed wiring publishes nothing and registers no cluster client, so the
    // rollback shows up as the nothing-wired case: `resolve()` succeeds and the
    // first call names the profile (DESIGN.md).
    let Ok(cache) = ClusterCacheV1::resolver(&hub)
        .profile(EventBroker)
        .resolve()
        .await
    else {
        panic!("an empty hub must not fail resolution");
    };
    assert!(
        matches!(
            cache.get("k").await,
            Err(ClusterError::ProfileNotBound { .. })
        ),
        "the profile registered before the failure must be rolled back"
    );

    plugin.stop().await;
}

#[tokio::test]
async fn a_natively_bound_lock_needs_no_opt_in_over_a_weak_cache() {
    // The shape the Redis plugin actually ships: a weak cache with a *native*
    // lock bound over it. The lock guard never runs, because no CAS default is
    // auto-filled for that primitive — so only leader election needs the flag.
    // This is why the plugin's own operator YAML binds `lock: { provider: redis }`
    // rather than reaching for a second opt-in.
    let hub = Arc::new(ClientHub::new());
    let calls = Arc::new(AtomicUsize::new(0));

    let handle = ClusterWiring::builder(Arc::clone(&hub))
        .profile(
            EventBroker,
            ProfileBackends::new(MemoryCache::eventually_consistent())
                .allow_weak_leader_election()
                .with_lock(Arc::new(MarkerLockBackend {
                    // Stands in for a purpose-built lock (the Redis plugin's
                    // `SET NX PX` lease). Its own inner backend rides a
                    // linearizable cache, which is exactly the point: a native
                    // binding brings its own guarantees rather than inheriting the
                    // profile cache's.
                    inner: Arc::new(
                        CasBasedDistributedLockBackend::new(MemoryCache::linearizable())
                            .expect("lock backend over linearizable cache"),
                    ),
                    calls: Arc::clone(&calls),
                })),
        )
        .build_and_start()
        .expect("a native lock over a weak cache needs no lock opt-in");

    let lock = DistributedLockV1::resolver(&hub)
        .profile(EventBroker)
        .resolve()
        .await
        .expect("the native lock resolves");
    let acquired = lock
        .try_lock("shard-assignment", Duration::from_secs(5))
        .await;
    assert!(
        acquired.is_ok(),
        "the native lock's own backend rides a linearizable cache, so this uncontended \
         acquisition must succeed: {acquired:?}"
    );
    drop(acquired);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the natively-bound lock must serve the call, not a CAS default"
    );

    handle.stop().await;
}
