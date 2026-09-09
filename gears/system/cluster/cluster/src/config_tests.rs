//! Tests for the operator YAML schema and the config-driven wiring path, wired
//! against the real [`StandaloneCacheProvider`] from the plugin crate — the same
//! provider a host assembles into the registry in production.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use cluster_sdk::lock::{DistributedLockBackend, LockFeatures, LockGuard};
use cluster_sdk::{
    CacheCapability, ClusterCacheBackend, ClusterCacheProvider, ClusterCacheV1, ClusterError,
    ClusterLockProvider, ClusterProfile, DistributedLockV1, LeaderElectionV1, ProfileHealth,
    StopHook, WireCacheConsistency,
};
use standalone_cluster_plugin::StandaloneCacheProvider;
use toolkit::client_hub::ClientHub;

use crate::defaults::test_cache::MemoryCache;
use crate::domain::wiring::ClusterHandle;
use crate::{
    BoundProfile, ClusterConfig, ClusterWiring, InstanceId, ProfileRegistry, ProviderRegistry,
};

/// The step the gear's `start` takes after [`ClusterWiring::from_config`]:
/// publish the bound set and register the local client, which makes the
/// profiles resolvable in this process (DESIGN.md). A test
/// standing in for the gear has to do what the gear does.
///
/// The registry comes back because clearing it at shutdown is the other half of
/// that job — the gear's `stop` does it, and so does a test that asserts stop
/// unbinds.
fn publish(handle: &mut ClusterHandle, bound: Vec<Arc<BoundProfile>>) -> Arc<ProfileRegistry> {
    let profiles = Arc::new(ProfileRegistry::new());
    handle.publish(&profiles, bound);
    profiles
}

fn standalone_registry() -> ProviderRegistry {
    ProviderRegistry::new().with_cache_provider(Arc::new(StandaloneCacheProvider))
}

// The profile the config fixtures name; matches the `event-broker` YAML key.
#[derive(Clone, Copy)]
struct EventBroker;
impl ClusterProfile for EventBroker {
    const NAME: &'static str = "event-broker";
}

#[test]
fn parses_omit_default_profile() {
    let yaml = "
profiles:
  event-broker:
    cache: { provider: standalone }
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let profile = cfg.profiles.get("event-broker").expect("profile present");
    assert_eq!(profile.cache.provider, "standalone");
    assert!(profile.cache.options.is_empty(), "no extra options");
    assert!(profile.leader_election.is_none());
    assert!(profile.lock.is_none());
}

#[test]
fn parses_flattened_provider_options() {
    let yaml = "
profiles:
  event-broker:
    cache:
      provider: standalone
      sweep_interval_ms: 50
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let cache = &cfg.profiles["event-broker"].cache;
    assert_eq!(cache.provider, "standalone");
    assert_eq!(
        cache
            .options
            .get("sweep_interval_ms")
            .and_then(serde_json::Value::as_u64),
        Some(50),
        "provider-specific option flows into the flattened options map"
    );
}

#[test]
fn unknown_top_level_key_is_rejected() {
    // `deny_unknown_fields` on the profile catches operator typos.
    let yaml = "
profiles:
  event-broker:
    cache: { provider: standalone }
    leeder_election: { provider: standalone }
";
    let parsed: Result<ClusterConfig, _> = serde_saphyr::from_str(yaml);
    assert!(
        parsed.is_err(),
        "a misspelled primitive key must be rejected"
    );
}

// `fence_retention` (§5.8.1, item `L3`)

/// The key is written the way every other duration in platform config is, and
/// DESIGN §4.10's example YAML uses exactly this form.
#[test]
fn fence_retention_parses_a_humantime_duration() {
    let yaml = "
fence_retention: 1h
profiles:
  event-broker:
    cache: { provider: standalone }
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    assert_eq!(cfg.fence_retention, Some(Duration::from_hours(1)));
    assert_eq!(
        cfg.fence_retention().expect("an hour is valid"),
        Duration::from_hours(1)
    );
}

/// Omitting it is the common case, and it must not mean "no window".
#[test]
fn fence_retention_defaults_to_the_sdk_constant() {
    let yaml = "
profiles:
  event-broker:
    cache: { provider: standalone }
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    assert_eq!(cfg.fence_retention, None, "nothing was configured");
    assert_eq!(
        cfg.fence_retention().expect("the default is valid"),
        cluster_sdk::lease::FENCE_RETENTION_DEFAULT,
        "and the default is what the backends get"
    );
}

/// Zero is the one value that silently defeats the point: the record's physical
/// expiry collapses onto the lease deadline and the fence resets on the next
/// reap. It is refused, by name, before any backend is built.
#[tokio::test]
async fn a_zero_window_fails_startup_by_name() {
    let yaml = "
fence_retention: 0s
profiles:
  event-broker:
    cache: { provider: standalone }
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let err = cfg
        .fence_retention()
        .expect_err("a zero window must be rejected");
    let ClusterError::InvalidConfig { reason } = &err else {
        panic!("expected InvalidConfig, got {err:?}");
    };
    assert!(
        reason.contains("fence_retention"),
        "the error must name the key an operator has to change: {reason}"
    );

    // And the wiring refuses to build anything at all, rather than starting a
    // pool and failing later.
    let hub = Arc::new(ClientHub::new());
    let wired = ClusterWiring::from_config(hub, &cfg, &standalone_registry()).await;
    assert!(
        wired.is_err(),
        "from_config must fail before a backend is constructed"
    );
}

#[tokio::test]
async fn from_config_wires_all_three_then_stop_unbinds() {
    let yaml = "
profiles:
  event-broker:
    cache: { provider: standalone }
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let hub = Arc::new(ClientHub::new());

    let (mut handle, bound) =
        ClusterWiring::from_config(Arc::clone(&hub), &cfg, &standalone_registry())
            .await
            .expect("wiring starts from config");
    assert_eq!(
        bound.len(),
        1,
        "one configured profile is returned as bound"
    );
    let profiles = publish(&mut handle, bound);

    assert!(
        ClusterCacheV1::resolver(&hub)
            .profile(EventBroker)
            .require(CacheCapability::Linearizable)
            .resolve()
            .await
            .is_ok(),
        "the configured cache resolves"
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

    // Both halves of the gear's `stop`: clear the published set, then tear the
    // wiring down (§4.8 phases 3-4). The cluster client stays registered, so the
    // refusal names the profile rather than reporting nothing-wired.
    profiles.clear();
    handle.stop().await;

    assert!(matches!(
        ClusterCacheV1::resolver(&hub)
            .profile(EventBroker)
            .resolve()
            .await,
        Err(ClusterError::ProfileNotBound { .. })
    ));
}

#[tokio::test]
async fn from_config_returns_provider_identity_and_declared_features() {
    // DESIGN 5.1/5.2: the hub cannot answer "which provider serves this profile"
    // or "what does it declare", so the bound-profile set carries both. Provider
    // identity is the operator's name for the backend, not the Rust type name -
    // it is what reaches a consumer in `CapabilityNotMet { provider }` (5.5).
    let yaml = "
profiles:
  event-broker:
    cache: { provider: standalone }
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let hub = Arc::new(ClientHub::new());

    let (handle, bound) =
        ClusterWiring::from_config(Arc::clone(&hub), &cfg, &standalone_registry())
            .await
            .expect("wiring starts from config");

    let profile = &bound[0];
    assert_eq!(profile.name, "event-broker");
    assert_eq!(profile.descriptor().name, "event-broker");
    assert_eq!(
        profile.descriptor().cache.provider,
        "standalone",
        "the cache reports the configured provider name"
    );
    // Both omitted primitives ride the SDK default over this profile's cache, so
    // the provider serving them is the cache's provider - that is where their
    // lease records live.
    assert_eq!(profile.descriptor().lock.provider, "standalone");
    assert_eq!(profile.descriptor().leader_election.provider, "standalone");

    // Declared characteristics are read off the real backends, so a descriptor
    // cannot claim a capability the backend does not declare.
    assert_eq!(
        profile.descriptor().cache.consistency,
        WireCacheConsistency::Linearizable,
        "the standalone cache declares linearizable"
    );
    assert!(
        profile.descriptor().cache.features.prefix_watch,
        "the standalone cache declares a native prefix watch"
    );
    assert!(
        profile.descriptor().lock.features.linearizable,
        "the CAS default over a linearizable cache declares linearizable exclusion"
    );
    assert!(profile.descriptor().leader_election.features.linearizable);
    assert_eq!(
        profile.descriptor().health,
        ProfileHealth::Serving,
        "a profile whose backends all built reports Serving until a probe says otherwise"
    );

    handle.stop().await;
}

#[tokio::test]
async fn from_config_returns_per_primitive_instance_refs() {
    // DESIGN 5.3: the refs say which backend *instance* serves each primitive, so
    // sharing is observable. Within one profile an auto-filled SDK default is a
    // distinct instance layered over the cache instance; across two profiles
    // nothing is deduplicated yet, so two `standalone` caches are two instances.
    let yaml = "
profiles:
  event-broker:
    cache: { provider: standalone }
  scheduler:
    cache: { provider: standalone }
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let hub = Arc::new(ClientHub::new());

    let (handle, bound) =
        ClusterWiring::from_config(Arc::clone(&hub), &cfg, &standalone_registry())
            .await
            .expect("wiring starts from config");

    assert_eq!(bound.len(), 2, "both configured profiles come back bound");
    let broker_profile = bound
        .iter()
        .find(|p| p.name == "event-broker")
        .expect("event-broker is bound");
    let broker = &broker_profile.instances;
    let scheduler = &bound
        .iter()
        .find(|p| p.name == "scheduler")
        .expect("scheduler is bound")
        .instances;

    // Each id names the instance the profile actually holds - the bound set keeps
    // a strong reference to it, so the id cannot go stale while it is reachable.
    assert_eq!(broker.cache, InstanceId::of(&broker_profile.cache));

    assert_ne!(
        broker.cache, broker.lock,
        "the CAS default lock is its own instance over the cache instance"
    );
    assert_ne!(broker.lock, broker.leader_election);
    assert_ne!(
        broker.cache, scheduler.cache,
        "two profiles each build their own cache instance today (the instance cache is DESIGN 5.3, not yet wired)"
    );

    handle.stop().await;
}

#[tokio::test]
async fn from_config_unknown_provider_fails() {
    let yaml = "
profiles:
  event-broker:
    cache: { provider: redis }
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let hub = Arc::new(ClientHub::new());

    let result = ClusterWiring::from_config(Arc::clone(&hub), &cfg, &standalone_registry()).await;
    assert!(
        matches!(result, Err(ClusterError::InvalidConfig { .. })),
        "an unregistered provider must fail startup"
    );
    // No partial registration leaks past the failure. Since `K4` a failed wiring
    // leaves *nothing* in the hub - not even a cluster client - so the report is
    // the nothing-wired one: `resolve()` succeeds and the first call names the
    // profile (§4.9.1).
    let Ok(cache) = ClusterCacheV1::resolver(&hub)
        .profile(EventBroker)
        .resolve()
        .await
    else {
        panic!("an empty hub must not fail resolution");
    };
    assert!(matches!(
        cache.get("k").await,
        Err(ClusterError::ProfileNotBound { .. })
    ));
}

#[tokio::test]
async fn from_config_unknown_non_cache_provider_fails() {
    // Per-primitive routing is supported, but each primitive's registry is
    // independent: `standalone` registers a *cache* provider only, so naming it
    // for `leader_election` names nothing. That must fail loudly rather than
    // silently fall back to the SDK default and ignore the operator's intent.
    let yaml = "
profiles:
  event-broker:
    cache: { provider: standalone }
    leader_election: { provider: standalone }
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let hub = Arc::new(ClientHub::new());

    let result = ClusterWiring::from_config(Arc::clone(&hub), &cfg, &standalone_registry()).await;
    let Err(ClusterError::InvalidConfig { reason }) = result else {
        panic!("a non-cache binding naming an unregistered provider must be rejected");
    };
    assert!(
        reason.contains("leader_election") && reason.contains("standalone"),
        "the error must name the primitive and the missing provider, got: {reason}"
    );
    // No partial registration leaks past the failure. Since `K4` a failed wiring
    // leaves *nothing* in the hub - not even a cluster client - so the report is
    // the nothing-wired one: `resolve()` succeeds and the first call names the
    // profile (§4.9.1).
    let Ok(cache) = ClusterCacheV1::resolver(&hub)
        .profile(EventBroker)
        .resolve()
        .await
    else {
        panic!("an empty hub must not fail resolution");
    };
    assert!(matches!(
        cache.get("k").await,
        Err(ClusterError::ProfileNotBound { .. })
    ));
}

/// A native (non-cache) lock provider standing in for a shipped plugin's
/// purpose-built lock backend — the Postgres plugin's `PostgresLockProvider` is
/// the real one, but it needs a live database, so the wiring-contract test uses
/// a fake that records whether it was the instance actually invoked.
struct FakeNativeLockProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ClusterLockProvider for FakeNativeLockProvider {
    fn provider(&self) -> &'static str {
        "fake-native"
    }

    async fn build_lock(
        &self,
        _options: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Arc<dyn DistributedLockBackend>, StopHook), ClusterError> {
        let backend = Arc::new(FakeNativeLock {
            calls: Arc::clone(&self.calls),
        });
        Ok((backend, Box::new(|| Box::pin(async {}))))
    }
}

/// The backend [`FakeNativeLockProvider`] builds. Every entry point bumps the
/// shared counter, so a non-zero count proves the natively-bound backend — not
/// the CAS default the omit-default path would auto-fill — received the call.
struct FakeNativeLock {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl DistributedLockBackend for FakeNativeLock {
    fn features(&self) -> LockFeatures {
        LockFeatures::new(true)
    }

    async fn try_lock(&self, name: &str, _ttl: Duration) -> Result<LockGuard, ClusterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ClusterError::LockContended {
            name: name.to_owned(),
        })
    }

    async fn lock(
        &self,
        name: &str,
        _ttl: Duration,
        _timeout: Duration,
    ) -> Result<LockGuard, ClusterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ClusterError::LockContended {
            name: name.to_owned(),
        })
    }
}

#[tokio::test]
async fn from_config_wires_a_mixed_backend_profile() {
    // UC-004 / `cpt-cf-clst-fr-routing-per-primitive`: one profile, cache served
    // by one provider and lock served by a *different*, native provider, while
    // leader election still rides the omit-default
    // auto-wrap over the cache. All four must resolve, and the lock calls must
    // land on the natively-bound backend.
    let yaml = "
profiles:
  event-broker:
    cache: { provider: standalone }
    lock: { provider: fake-native }
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let hub = Arc::new(ClientHub::new());
    let lock_calls = Arc::new(AtomicUsize::new(0));
    let registry = standalone_registry().with_lock_provider(Arc::new(FakeNativeLockProvider {
        calls: Arc::clone(&lock_calls),
    }));

    let (mut handle, bound) = ClusterWiring::from_config(Arc::clone(&hub), &cfg, &registry)
        .await
        .expect("a mixed-backend profile must wire");

    // Per-primitive routing shows up in the descriptor: the lock names its own
    // provider while leader election still names the cache it rides.
    let descriptor = bound[0].descriptor();
    let _profiles = publish(&mut handle, bound.clone());
    assert_eq!(descriptor.cache.provider, "standalone");
    assert_eq!(descriptor.lock.provider, "fake-native");
    assert_eq!(descriptor.leader_election.provider, "standalone");
    assert!(
        descriptor.lock.features.linearizable,
        "the native lock's own declared features are reported, not the CAS default's"
    );

    // All three primitives resolve under the one profile, per the requirement's
    // "consumer gears see four working primitives" clause.
    assert!(
        ClusterCacheV1::resolver(&hub)
            .profile(EventBroker)
            .resolve()
            .await
            .is_ok(),
        "the mixed profile's cache resolves"
    );
    assert!(
        LeaderElectionV1::resolver(&hub)
            .profile(EventBroker)
            .resolve()
            .await
            .is_ok(),
        "leader election still rides the omit-default auto-wrap over the cache"
    );

    let lock = DistributedLockV1::resolver(&hub)
        .profile(EventBroker)
        .resolve()
        .await
        .expect("the natively-bound lock resolves");
    // `LockContended` is the fake's canned answer; what matters is which instance
    // answered. The CAS default over a fresh standalone cache would have
    // granted this uncontended lock instead.
    assert!(
        matches!(
            lock.try_lock("shard-assignment", Duration::from_secs(5))
                .await,
            Err(ClusterError::LockContended { .. })
        ),
        "the natively-bound lock backend must serve the call, not the CAS default"
    );
    assert_eq!(
        lock_calls.load(Ordering::SeqCst),
        1,
        "the native lock backend must be the registered instance"
    );

    handle.stop().await;
}

// ---------------------------------------------------------------------------
// The reserved `provider: default` binding and the weak-consistency opt-in
// (`config::DEFAULT_PROVIDER` / `config::DefaultBindingOptions`).
//
// These exist for the Redis backend, which is the first cache in the workspace
// to declare `EventuallyConsistent` — see
// `plugins/redis-cluster-plugin/docs/DESIGN.md` §13 D1. Both CAS-based
// SDK defaults reject such a cache, so without an opt-in the `Redis-only`
// deployment shape in `gears/system/cluster/docs/DESIGN.md` §4.2 cannot start at
// all. `MemoryCache::eventually_consistent` stands in for the Redis cache, so
// none of this needs a container.
// ---------------------------------------------------------------------------

/// A cache provider whose backend declares `EventuallyConsistent`, standing in for
/// the Redis cache in every replicated or non-fsync-durable configuration.
struct WeakCacheProvider;

#[async_trait]
impl ClusterCacheProvider for WeakCacheProvider {
    fn provider(&self) -> &'static str {
        "weak-cache"
    }

    async fn build_cache(
        &self,
        _options: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Arc<dyn ClusterCacheBackend>, StopHook), ClusterError> {
        Ok((
            MemoryCache::eventually_consistent(),
            Box::new(|| Box::pin(async {})),
        ))
    }
}

fn weak_cache_registry() -> ProviderRegistry {
    ProviderRegistry::new().with_cache_provider(Arc::new(WeakCacheProvider))
}

/// A cache provider whose stop hook records that it ran, so a test can assert a
/// failed wiring shut down the backends it had already started.
struct StoppableCacheProvider(Arc<AtomicUsize>);

#[async_trait]
impl ClusterCacheProvider for StoppableCacheProvider {
    fn provider(&self) -> &'static str {
        "stoppable-weak-cache"
    }

    async fn build_cache(
        &self,
        _options: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Arc<dyn ClusterCacheBackend>, StopHook), ClusterError> {
        let stops = Arc::clone(&self.0);
        Ok((
            MemoryCache::eventually_consistent(),
            Box::new(move || {
                Box::pin(async move {
                    stops.fetch_add(1, Ordering::SeqCst);
                })
            }),
        ))
    }
}

#[tokio::test]
async fn a_wiring_that_fails_stops_the_backends_it_already_started() {
    // The regression test for the unwind DESIGN.md §3.13 describes.
    // `from_config` builds each profile's cache — opening real pools,
    // tasks, and connections — before it can discover that an *omitted* primitive
    // cannot be auto-filled over that cache. On that failure it used to drop the
    // builder, discarding the stop hooks without running them, so every backend it
    // had started stayed running for the life of the process.
    //
    // It went unnoticed because the leak is quiet for the backends that existed at
    // the time: an idle Postgres pool. The Redis plugin has an ADR-006 `Drop`
    // guard, so the same leak *panics in a debug build* — which is how it was
    // found, by a scenario (`RD-SPEC-004`) whose entire subject is a profile that
    // must fail startup.
    //
    // A weak cache with `leader_election` omitted is the smallest way to reach it:
    // the omit-default is a CAS backend whose consistency guard rejects the cache,
    // and that rejection happens after `build_cache` has returned.
    let stops = Arc::new(AtomicUsize::new(0));
    let providers = ProviderRegistry::new()
        .with_cache_provider(Arc::new(StoppableCacheProvider(Arc::clone(&stops))));
    let yaml = "
profiles:
  event-broker:
    cache: { provider: stoppable-weak-cache }
";
    let config: ClusterConfig = serde_saphyr::from_str(yaml).expect("the profile config parses");
    let outcome = ClusterWiring::from_config(Arc::new(ClientHub::new()), &config, &providers).await;

    assert!(
        outcome.is_err(),
        "a weak cache with leader_election omitted must fail startup - that is the default-off \
         behaviour the opt-in exists to override"
    );
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "and the cache backend it had already started must be stopped before the error is \
         reported, not leaked for the life of the process"
    );
}

#[test]
fn parses_a_default_provider_binding() {
    let yaml = "
profiles:
  event-broker:
    cache: { provider: weak-cache }
    leader_election:
      provider: default
      allow_weak_consistency: true
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let binding = cfg.profiles["event-broker"]
        .leader_election
        .as_ref()
        .expect("the binding is present");
    assert_eq!(binding.provider, "default");
    assert_eq!(
        binding
            .options
            .get("allow_weak_consistency")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "the flag rides the flattened options map like any provider option"
    );
}

#[tokio::test]
async fn weak_cache_profile_fails_without_the_opt_in() {
    // The default-off behaviour ADR-009 wants, and the behaviour an operator hits
    // today when they bind Redis and omit the other primitives. Pinned so the
    // opt-in cannot quietly become the default.
    let yaml = "
profiles:
  event-broker:
    cache: { provider: weak-cache }
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let hub = Arc::new(ClientHub::new());

    let result = ClusterWiring::from_config(Arc::clone(&hub), &cfg, &weak_cache_registry()).await;
    let Err(ClusterError::InvalidConfig { reason }) = result else {
        panic!("an eventually-consistent cache must not silently get the CAS defaults");
    };
    assert!(
        reason.contains("linearizable") && reason.contains("new_allow_weak_consistency"),
        "the error must say what is required and name the opt-in, got: {reason}"
    );
}

#[tokio::test]
async fn opting_in_on_leader_election_alone_still_fails_on_the_lock() {
    // The regression this pair of flags exists for. `CasBasedDistributedLockBackend::new`
    // shares `reject_weak_consistency` with the leader default, and the leader is
    // resolved first — so waiving only the leader guard moves the startup failure
    // a few lines down rather than resolving it. A single-flag implementation
    // passes every leader-election test and fails here.
    let yaml = "
profiles:
  event-broker:
    cache: { provider: weak-cache }
    leader_election:
      provider: default
      allow_weak_consistency: true
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let hub = Arc::new(ClientHub::new());

    let result = ClusterWiring::from_config(Arc::clone(&hub), &cfg, &weak_cache_registry()).await;
    let Err(ClusterError::InvalidConfig { reason }) = result else {
        panic!("the lock default's own consistency guard must still bite");
    };
    assert!(
        reason.contains("CasBasedDistributedLockBackend"),
        "the surviving failure must be the *lock* guard, not the leader one, got: {reason}"
    );
}

#[tokio::test]
async fn opting_in_on_both_cas_defaults_starts_a_weak_cache_profile() {
    // The `Redis-only` shape, expressible at last: all three primitives resolve
    // over an eventually-consistent cache once the operator has acknowledged the
    // split-brain risk on each guarded primitive.
    let yaml = "
profiles:
  event-broker:
    cache: { provider: weak-cache }
    leader_election:
      provider: default
      allow_weak_consistency: true
    lock:
      provider: default
      allow_weak_consistency: true
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let hub = Arc::new(ClientHub::new());

    let (mut handle, bound) =
        ClusterWiring::from_config(Arc::clone(&hub), &cfg, &weak_cache_registry())
            .await
            .expect("both opt-ins present, so the profile must start");
    let _profiles = publish(&mut handle, bound);

    for resolved in [
        ClusterCacheV1::resolver(&hub)
            .profile(EventBroker)
            .resolve()
            .await
            .is_ok(),
        LeaderElectionV1::resolver(&hub)
            .profile(EventBroker)
            .resolve()
            .await
            .is_ok(),
        DistributedLockV1::resolver(&hub)
            .profile(EventBroker)
            .resolve()
            .await
            .is_ok(),
    ] {
        assert!(resolved, "every primitive must resolve under the opt-in");
    }

    // The flag waives a *constructor guard*; it does not launder capability
    // validation. A consumer that declares it needs linearizable CAS still fails
    // startup, because the cache's declaration has not changed and nothing about
    // this flag makes it change.
    assert!(
        matches!(
            ClusterCacheV1::resolver(&hub)
                .profile(EventBroker)
                .require(CacheCapability::Linearizable)
                .resolve()
                .await,
            Err(ClusterError::CapabilityNotMet { .. })
        ),
        "the opt-in must not make a weak cache satisfy CacheCapability::Linearizable"
    );

    handle.stop().await;
}

#[tokio::test]
async fn the_default_provider_is_never_looked_up_in_the_registry() {
    // `default` is a reserved name the wiring resolves itself, not a registry
    // entry. If the interception were ordered after the lookup, a plugin that
    // registered a provider called `default` would silently take over the
    // omit-default path for every profile that named it.
    // Both guarded primitives opt in: the impostor squats on `lock`, but a profile
    // over a weak cache still has to waive the leader guard to start at all.
    let yaml = "
profiles:
  event-broker:
    cache: { provider: weak-cache }
    leader_election:
      provider: default
      allow_weak_consistency: true
    lock:
      provider: default
      allow_weak_consistency: true
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let hub = Arc::new(ClientHub::new());
    let impostor_calls = Arc::new(AtomicUsize::new(0));
    let registry =
        weak_cache_registry().with_lock_provider(Arc::new(ImpostorDefaultLockProvider {
            calls: Arc::clone(&impostor_calls),
        }));

    let (mut handle, bound) = ClusterWiring::from_config(Arc::clone(&hub), &cfg, &registry)
        .await
        .expect("the reserved name resolves without the registry");
    let _profiles = publish(&mut handle, bound);

    assert_eq!(
        impostor_calls.load(Ordering::SeqCst),
        0,
        "a provider registered under the reserved name must never be built"
    );

    // And the backend actually bound is the SDK default over the cache, not the
    // impostor: the CAS default grants an uncontended lock, the impostor refuses
    // every acquisition.
    let lock = DistributedLockV1::resolver(&hub)
        .profile(EventBroker)
        .resolve()
        .await
        .expect("the lock resolves");
    assert!(
        lock.try_lock("shard-assignment", Duration::from_secs(5))
            .await
            .is_ok(),
        "the SDK default must serve the call, not a registry entry named `default`"
    );

    handle.stop().await;
}

/// A lock provider squatting on the reserved [`crate::config::DEFAULT_PROVIDER`]
/// name. Registering it is legal — nothing stops a plugin choosing that string —
/// so the wiring's interception order is what has to keep it unreachable.
struct ImpostorDefaultLockProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ClusterLockProvider for ImpostorDefaultLockProvider {
    fn provider(&self) -> &'static str {
        "default"
    }

    async fn build_lock(
        &self,
        _options: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Arc<dyn DistributedLockBackend>, StopHook), ClusterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let backend = Arc::new(FakeNativeLock {
            calls: Arc::clone(&self.calls),
        });
        Ok((backend, Box::new(|| Box::pin(async {}))))
    }
}

#[tokio::test]
async fn cache_bound_to_the_default_provider_is_rejected() {
    // `default` means "the SDK default backend *over* a profile's cache", so it
    // cannot name the cache itself — there would be nothing left to wrap.
    let yaml = "
profiles:
  event-broker:
    cache: { provider: default }
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let hub = Arc::new(ClientHub::new());

    let result = ClusterWiring::from_config(Arc::clone(&hub), &cfg, &weak_cache_registry()).await;
    let Err(ClusterError::InvalidConfig { reason }) = result else {
        panic!("`cache: {{ provider: default }}` must be rejected");
    };
    assert!(
        reason.contains("cache") && !reason.contains("unknown cache provider"),
        "the error must explain the reserved name rather than report it as unknown, got: {reason}"
    );
}

#[tokio::test]
async fn a_misspelled_option_on_a_default_binding_is_rejected() {
    // These options reach no provider, so this is the only layer that can catch
    // the typo. Silently ignoring it would leave the profile failing startup with
    // the consistency-guard error, which says nothing about the misspelling.
    let yaml = "
profiles:
  event-broker:
    cache: { provider: weak-cache }
    leader_election:
      provider: default
      allow_weak_consistancy: true
";
    let cfg: ClusterConfig = serde_saphyr::from_str(yaml).expect("config parses");
    let hub = Arc::new(ClientHub::new());

    let result = ClusterWiring::from_config(Arc::clone(&hub), &cfg, &weak_cache_registry()).await;
    let Err(ClusterError::InvalidConfig { reason }) = result else {
        panic!("an unknown option on a `default` binding must be rejected");
    };
    assert!(
        reason.contains("allow_weak_consistancy") && reason.contains("allow_weak_consistency"),
        "the error must show both the typo and the accepted spelling, got: {reason}"
    );
}
