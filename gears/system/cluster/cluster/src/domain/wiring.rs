//! The cluster wiring builder, per-profile backend bindings, and lifecycle
//! handle (DESIGN §3.7).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use cluster_sdk::{
    CacheDescriptor, ClusterCacheBackend, ClusterClient, ClusterError, ClusterProfile,
    DistributedLockBackend, LeaderElectionBackend, LeaderElectionDescriptor, LockDescriptor,
    ProfileDescriptor, ProfileHealth, StopHook, deregister_cache_backend,
    deregister_leader_election_backend, deregister_lock_backend, register_cache_backend,
    register_leader_election_backend, register_lock_backend,
};

use crate::defaults::{
    CasBasedDistributedLockBackend, CasBasedLeaderElectionBackend, ShutdownRevoke,
};
use toolkit::client_hub::ClientHub;

use crate::config::{BackendBinding, ClusterConfig, DEFAULT_PROVIDER, ProfileConfig};
use crate::domain::local_client::LocalClusterClient;
use crate::domain::provider::ProviderRegistry;
use crate::domain::registry::{BoundProfile, InstanceId, ProfileInstanceRefs, ProfileRegistry};

/// The health a freshly wired profile declares until the composite readiness
/// healthcheck probes it (DESIGN §4.4).
///
/// Wiring is the only evidence available at this point, and it is positive: every
/// backend the profile binds was constructed successfully, so `Serving` is the
/// honest reading and a failing `probe()` is what can later contradict it. It is
/// deliberately *not* the enum's `Default` (`Degraded`), which is the wire
/// fail-safe for a health value that arrived unspecified — a different question
/// from a profile whose backends have just been built.
const WIRED_HEALTH: ProfileHealth = ProfileHealth::Serving;

/// What wiring the cluster produces: the lifecycle [`ClusterHandle`] and the
/// bound-profile set the profile registry publishes (DESIGN §5.1, §5.2).
pub type WiredCluster = (ClusterHandle, Vec<Arc<BoundProfile>>);

/// The per-primitive backend bindings for one profile.
///
/// `cache` is required; each of the other two primitives may be bound to its
/// own backend (`cpt-cf-clst-fr-routing-per-primitive`) or left `None`, in which
/// case [`ClusterWiringBuilder::build_and_start`] auto-fills it with the SDK
/// default backend over `cache` (`cpt-cf-clst-fr-routing-omit-default`).
pub struct ProfileBackends {
    cache: Arc<dyn ClusterCacheBackend>,
    leader_election: Option<Arc<dyn LeaderElectionBackend>>,
    lock: Option<Arc<dyn DistributedLockBackend>>,
    /// The operator-facing provider names behind these backends, which only the
    /// config-driven path knows ([`ClusterWiring::from_config`]). `None` on the
    /// programmatic builder path, where the resolved backends' own
    /// `provider_name()` is the only identity there is.
    providers: Option<ProviderIdentities>,
    /// Whether an auto-filled [`CasBasedLeaderElectionBackend`] may be built over
    /// an eventually-consistent cache, via
    /// [`ProfileBackends::allow_weak_leader_election`]. Ignored when
    /// `leader_election` is explicitly bound to a native backend.
    allow_weak_leader_election: bool,
    /// Whether an auto-filled [`CasBasedDistributedLockBackend`] may be built over
    /// an eventually-consistent cache, via [`ProfileBackends::allow_weak_lock`].
    /// Ignored when `lock` is explicitly bound to a native backend.
    allow_weak_lock: bool,
}

/// The provider name behind each primitive of one profile, as the operator wrote
/// it — `"postgres"`, not `postgres_cluster_plugin::cache::PostgresCache`.
///
/// This is the identity that reaches a consumer, on the descriptor and so in
/// `CapabilityNotMet { provider }`: an operator reading a capability failure must
/// see which real backend failed the requirement (DESIGN §5.5).
struct ProviderIdentities {
    cache: String,
    /// The provider of the native leader-election binding, or — when the
    /// primitive was omitted and rides the SDK default — the provider of the
    /// cache that default is layered over, since that cache is what stores the
    /// election's lease records.
    leader_election: String,
    /// The lock's provider, on the same rule as
    /// [`leader_election`](Self::leader_election).
    lock: String,
}

impl ProviderIdentities {
    /// The identities to report when no configured provider name exists — the
    /// programmatic builder path. `provider_name()` resolves through the vtable
    /// to the concrete backend type, which is the same identity
    /// `CapabilityNotMet` already carries today.
    fn from_backends(
        cache: &Arc<dyn ClusterCacheBackend>,
        leader_election: &Arc<dyn LeaderElectionBackend>,
        lock: &Arc<dyn DistributedLockBackend>,
    ) -> Self {
        Self {
            cache: cache.provider_name().to_owned(),
            leader_election: leader_election.provider_name().to_owned(),
            lock: lock.provider_name().to_owned(),
        }
    }
}

impl ProfileBackends {
    /// Binds a profile to `cache`, leaving the other two primitives to the SDK
    /// defaults unless overridden with the `with_*` methods.
    #[must_use]
    pub fn new(cache: Arc<dyn ClusterCacheBackend>) -> Self {
        Self {
            cache,
            leader_election: None,
            lock: None,
            providers: None,
            allow_weak_leader_election: false,
            allow_weak_lock: false,
        }
    }

    /// Permits the auto-filled leader-election default to be built over an
    /// eventually-consistent cache, routing it through
    /// [`CasBasedLeaderElectionBackend::new_allow_weak_consistency`] instead of the
    /// default-safe `new`.
    ///
    /// Takes no argument on purpose: calling it *is* the acknowledgement, mirroring
    /// the SDK constructor it selects, so there is no `false` to pass and no way to
    /// reach the weak path without naming it. Without this, a profile whose cache
    /// declares `EventuallyConsistent` fails
    /// [`build_and_start`](ClusterWiringBuilder::build_and_start) with
    /// `InvalidConfig` — which is the correct default (ADR-009), not a bug to work
    /// around.
    ///
    /// The config-driven equivalent is
    /// `leader_election: { provider: default, allow_weak_consistency: true }`.
    /// No-op when `leader_election` is bound to a native backend, which brings its
    /// own guarantees.
    #[must_use]
    pub fn allow_weak_leader_election(mut self) -> Self {
        self.allow_weak_leader_election = true;
        self
    }

    /// Permits the auto-filled lock default to be built over an
    /// eventually-consistent cache, routing it through
    /// [`CasBasedDistributedLockBackend::new_allow_weak_consistency`].
    ///
    /// The lock counterpart of
    /// [`allow_weak_leader_election`](Self::allow_weak_leader_election), and needed
    /// just as often: both CAS-based defaults share the same constructor guard, so a
    /// weak-cache profile that only permits the weak leader election still fails on
    /// the lock. The config-driven equivalent is
    /// `lock: { provider: default, allow_weak_consistency: true }`.
    ///
    /// A profile that can bind a *native* lock should prefer that — the Redis
    /// plugin's `SET NX PX` lease is a purpose-built primitive, whereas this flag
    /// only silences a guard over CAS on a cache that cannot support it.
    #[must_use]
    pub fn allow_weak_lock(mut self) -> Self {
        self.allow_weak_lock = true;
        self
    }

    /// Binds a native leader-election backend, overriding the SDK default.
    #[must_use]
    pub fn with_leader_election(mut self, backend: Arc<dyn LeaderElectionBackend>) -> Self {
        self.leader_election = Some(backend);
        self
    }

    /// Binds a native distributed-lock backend, overriding the SDK default.
    #[must_use]
    pub fn with_lock(mut self, backend: Arc<dyn DistributedLockBackend>) -> Self {
        self.lock = Some(backend);
        self
    }
}

/// Entry point for wiring the cluster gear.
pub struct ClusterWiring;

impl ClusterWiring {
    /// Returns a builder that registers backends into `hub`.
    ///
    /// `hub` is taken as a shared [`Arc`] (rather than a borrow) so the returned
    /// [`ClusterHandle`] can outlive the call and deregister at
    /// [`stop`](ClusterHandle::stop) time.
    pub fn builder(hub: Arc<ClientHub>) -> ClusterWiringBuilder {
        ClusterWiringBuilder {
            hub,
            profiles: Vec::new(),
            stop_hooks: Vec::new(),
            fence_retention: cluster_sdk::lease::FENCE_RETENTION_DEFAULT,
        }
    }

    /// Builds the wiring from operator [`ClusterConfig`], instantiating each
    /// profile's cache backend through the matching provider in `providers` and
    /// letting the omit-default auto-wrap supply the other two primitives.
    ///
    /// Each provider's shutdown hook is owned by the returned [`ClusterHandle`]
    /// and awaited on [`stop`](ClusterHandle::stop).
    ///
    /// # The bound-profile set
    ///
    /// Returns the [`BoundProfile`] set alongside the handle (DESIGN §5.1, §5.2).
    /// Hub registration under `cluster:{profile}` and the all-or-nothing rollback
    /// are unchanged — this is the profile knowledge the hub cannot enumerate,
    /// returned so the profile registry has a data source: per-profile provider
    /// identity, the declared consistency and features each backend reports, and
    /// which instances the profile is built from.
    ///
    /// The set holds strong `Arc`s to those instances, so keeping it alive keeps
    /// the profiles' backends alive independently of the hub registrations
    /// (§5.3).
    ///
    /// # Errors
    /// - [`ClusterError::InvalidConfig`] if a profile names an unregistered
    ///   provider for any primitive, or if a provider rejects its options.
    /// - Propagates [`ClusterError`] from provider construction, the SDK default
    ///   backends (consistency guard), and backend registration (invalid name).
    pub async fn from_config(
        hub: Arc<ClientHub>,
        config: &ClusterConfig,
        providers: &ProviderRegistry,
    ) -> Result<WiredCluster, ClusterError> {
        // Wiring is all-or-nothing, and on the failure path that means *shutting
        // down what already started* rather than merely not returning a handle.
        // Every `?` in `wire_profiles`/`build_and_start_bound_returning_hooks` can
        // fire after one or more profiles have had real backends built — a
        // `build_cache_for_profile` opens a connection pool, background tasks, and
        // (for Redis) a second subscriber connection — and those live behind stop
        // hooks accumulated on `builder`. Dropping the builder discards the hooks
        // without running them, so a misconfigured profile leaked every backend it
        // managed to build before failing.
        //
        // Invisible for a long time, because the Postgres plugin's leak is a quiet
        // idle pool. The Redis plugin has an ADR-006 `Drop` guard, so the same
        // leak **panics in a debug build** — which is how this was found, by
        // `RD-SPEC-004`, whose whole subject is a profile that must fail startup
        // (redis-cluster-plugin/docs/TESTING.md §4.6).
        //
        // `fence_retention` is read before any backend is built: a zero window is
        // an operator error that must fail before a pool is opened, and with no
        // hooks yet accumulated a plain `?` here has nothing to unwind (§5.8.1).
        let builder = Self::builder(hub).with_fence_retention(config.fence_retention()?)?;
        let builder = match Self::wire_profiles(builder, config, providers).await {
            Ok(builder) => builder,
            Err((err, hooks)) => {
                run_stop_hooks_on_failed_wiring(hooks).await;
                return Err(err);
            }
        };
        match builder.build_and_start_bound_returning_hooks() {
            Ok(wired) => Ok(wired),
            Err((err, hooks)) => {
                run_stop_hooks_on_failed_wiring(hooks).await;
                Err(err)
            }
        }
    }

    /// Wires every profile in `config` onto `builder`.
    ///
    /// Hands the accumulated [`StopHook`]s back alongside the error rather than
    /// letting them drop, so [`from_config`](Self::from_config) can shut down the
    /// backends earlier profiles already started. That is the only reason this is a
    /// separate function: the loop body is otherwise unchanged, and `?` inside it
    /// would discard exactly what the caller needs.
    async fn wire_profiles(
        mut builder: ClusterWiringBuilder,
        config: &ClusterConfig,
        providers: &ProviderRegistry,
    ) -> Result<ClusterWiringBuilder, (ClusterError, Vec<StopHook>)> {
        macro_rules! unwind_on_err {
            ($result:expr, $builder:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(err) => return Err((err, $builder.take_stop_hooks())),
                }
            };
        }

        for (name, profile) in &config.profiles {
            tracing::debug!(profile = %name, "wiring cluster profile from config");
            let (cache, cache_stop) = unwind_on_err!(
                build_cache_for_profile(name, profile, providers).await,
                builder
            );
            // Pushed immediately, so it matches the cache's actual start-order
            // position (first). `build_and_start` runs `stop_hooks` in reverse push
            // order, so pushing here — before the leader/lock hooks below — means
            // the cache stops LAST, after every primitive layered on top of it for
            // this profile (true reverse-start order, DESIGN §3.7).
            builder = builder.on_stop(move || async move { cache_stop().await });

            let mut backends = ProfileBackends::new(Arc::clone(&cache));
            // Recorded before the bindings are resolved, because it is the
            // *config* that says which provider serves each primitive, and an
            // omitted primitive — or a `default` sentinel binding, which the SDK
            // default still layers over this cache — inherits the cache's provider
            // (§5.5). `binding_provider` folds the sentinel into that omitted case.
            backends.providers = Some(ProviderIdentities {
                cache: profile.cache.provider.clone(),
                leader_election: binding_provider(
                    profile.leader_election.as_ref(),
                    &profile.cache.provider,
                ),
                lock: binding_provider(profile.lock.as_ref(), &profile.cache.provider),
            });

            // The two optional primitives, each either natively bound through its
            // own provider registry or riding the omit-default auto-wrap, and each
            // honouring the reserved [`DEFAULT_PROVIDER`] sentinel. One helper
            // apiece rather than two near-identical blocks inline: they differ only
            // in which registry they consult and which `with_*` setter they call.
            //
            // Each appends its plugin stop hook to `hooks` rather than pushing
            // straight onto the builder, so a failure in a later binding can unwind
            // the earlier ones too — `bind_lock` failing after `bind_leader_election`
            // started a native leader backend has to stop it. The local hooks are
            // handed to the builder before any error is returned, which is what
            // `take_stop_hooks` then collects.
            let mut hooks: Vec<StopHook> = Vec::new();
            let bound = async {
                let backends =
                    bind_leader_election(name, profile, providers, backends, &mut hooks).await?;
                bind_lock(name, profile, providers, backends, &mut hooks).await
            }
            .await;
            for stop in hooks {
                builder = builder.on_stop(move || async move { stop().await });
            }
            backends = unwind_on_err!(bound, builder);

            builder = builder.profile_named(name.clone(), backends);
        }
        Ok(builder)
    }
}

/// Runs the stop hooks of a wiring that failed partway, newest first.
///
/// Reverse push order, the same order [`ClusterHandle::stop`] uses, so a
/// primitive layered on a cache is stopped before the cache it rides. Failures
/// are not propagated: the caller already has an error to report, and it is the
/// *configuration* error an operator needs to see rather than whatever a
/// best-effort teardown then said.
async fn run_stop_hooks_on_failed_wiring(hooks: Vec<StopHook>) {
    if hooks.is_empty() {
        return;
    }
    tracing::debug!(
        hooks = hooks.len(),
        "cluster wiring failed after starting backends; stopping them before reporting"
    );
    for hook in hooks.into_iter().rev() {
        hook().await;
    }
}

/// Binds a profile's `leader_election`, honouring the reserved
/// [`DEFAULT_PROVIDER`] sentinel.
///
/// The sentinel is checked *before* the registry lookup, so the reserved name can
/// never be shadowed by a plugin that registers a provider called `default`. It
/// contributes no backend and no stop hook — [`resolve_profile_backends`] still
/// supplies the SDK default and owns its shutdown-revoke seam; all the binding does
/// is carry the operator's `allow_weak_consistency` acknowledgement to it.
async fn bind_leader_election(
    name: &str,
    profile: &ProfileConfig,
    providers: &ProviderRegistry,
    mut backends: ProfileBackends,
    hooks: &mut Vec<StopHook>,
) -> Result<ProfileBackends, ClusterError> {
    let Some(binding) = &profile.leader_election else {
        return Ok(backends);
    };
    if binding.provider == DEFAULT_PROVIDER {
        if guarded_default_options(name, "leader_election", binding)?.allow_weak_consistency {
            backends = backends.allow_weak_leader_election();
        }
        return Ok(backends);
    }
    let provider = providers
        .leader_election_provider(&binding.provider)
        .ok_or_else(|| ClusterError::InvalidConfig {
            reason: format!(
                "profile `{name}`: unknown leader_election provider `{}`",
                binding.provider
            ),
        })?;
    let (backend, stop) = provider.build_leader_election(&binding.options).await?;
    hooks.push(stop);
    Ok(backends.with_leader_election(backend))
}

/// Binds a profile's `lock`, honouring the reserved [`DEFAULT_PROVIDER`] sentinel.
/// See [`bind_leader_election`] for how the sentinel is handled.
async fn bind_lock(
    name: &str,
    profile: &ProfileConfig,
    providers: &ProviderRegistry,
    mut backends: ProfileBackends,
    hooks: &mut Vec<StopHook>,
) -> Result<ProfileBackends, ClusterError> {
    let Some(binding) = &profile.lock else {
        return Ok(backends);
    };
    if binding.provider == DEFAULT_PROVIDER {
        if guarded_default_options(name, "lock", binding)?.allow_weak_consistency {
            backends = backends.allow_weak_lock();
        }
        return Ok(backends);
    }
    let provider =
        providers
            .lock_provider(&binding.provider)
            .ok_or_else(|| ClusterError::InvalidConfig {
                reason: format!(
                    "profile `{name}`: unknown lock provider `{}`",
                    binding.provider
                ),
            })?;
    let (backend, stop) = provider.build_lock(&binding.options).await?;
    hooks.push(stop);
    Ok(backends.with_lock(backend))
}

/// The options a `provider: `[`DEFAULT_PROVIDER`] binding may carry, for a primitive
/// whose SDK default backend has a consistency guard (`leader_election` and `lock`).
///
/// Lives here rather than in [`crate::config`] because it is a wiring-internal
/// parsing detail: unlike every other binding option, these keys never reach a
/// provider, so nothing outside this module ever sees the parsed form. The
/// operator-facing half — what the flag means and where it is accepted — is
/// documented on [`DEFAULT_PROVIDER`].
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultBindingOptions {
    /// Route this primitive's SDK default through `new_allow_weak_consistency`
    /// instead of the default-safe `new`, accepting that an eventually-consistent
    /// cache may produce split-brain (dual leaders / dual lock holders) under
    /// partition (ADR-009). Default `false`.
    #[serde(default)]
    allow_weak_consistency: bool,
}

/// Parses a `provider: `[`DEFAULT_PROVIDER`] binding's options for one of the two
/// primitives whose SDK default carries a consistency guard.
///
/// `deny_unknown_fields` on [`DefaultBindingOptions`] does the work: these options
/// reach no provider, so this is the only layer that can catch an operator's typo,
/// and a silently-ignored `allow_weak_consistancy` would leave the profile failing
/// startup with an error that says nothing about the misspelling.
fn guarded_default_options(
    profile: &str,
    primitive: &str,
    binding: &BackendBinding,
) -> Result<DefaultBindingOptions, ClusterError> {
    serde_json::from_value(serde_json::Value::Object(binding.options.clone())).map_err(|err| {
        ClusterError::InvalidConfig {
            reason: format!(
                "profile `{profile}`: invalid options on `{primitive}: {{ provider: \
                 {DEFAULT_PROVIDER} }}`: {err}. The only option this binding accepts is \
                 `allow_weak_consistency: <bool>`"
            ),
        }
    })
}

/// The provider serving a primitive: its own binding's when it names a real one,
/// else the `cache_provider` the omit-default SDK backend is layered over (§5.5).
///
/// A `provider: `[`DEFAULT_PROVIDER`] binding reports the cache provider too, not
/// the literal `default`: the sentinel selects the same SDK default an omitted
/// primitive gets, layered over this profile's cache, so the identity a consumer
/// sees in `CapabilityNotMet { provider }` is that cache's — the backend that
/// actually stores the coordination records.
fn binding_provider(binding: Option<&BackendBinding>, cache_provider: &str) -> String {
    match binding {
        Some(binding) if binding.provider != DEFAULT_PROVIDER => binding.provider.clone(),
        _ => cache_provider.to_owned(),
    }
}

async fn build_cache_for_profile(
    name: &str,
    profile: &ProfileConfig,
    providers: &ProviderRegistry,
) -> Result<(Arc<dyn ClusterCacheBackend>, StopHook), ClusterError> {
    // The cache is the anchor every SDK default wraps, so there is no "default
    // cache" for the reserved name to resolve to. Caught here rather than left to
    // the registry lookup, which would answer the misleading "unknown cache
    // provider `default`" — misleading because `default` *is* a name the wiring
    // knows, just not one this binding can use.
    if profile.cache.provider == DEFAULT_PROVIDER {
        return Err(ClusterError::InvalidConfig {
            reason: format!(
                "profile `{name}`: `cache: {{ provider: {DEFAULT_PROVIDER} }}` is not valid — \
                 `{DEFAULT_PROVIDER}` selects the SDK default backend *over* a profile's cache, \
                 so the cache itself must name a real provider (e.g. `standalone`, `postgres`, \
                 `redis`)"
            ),
        });
    }
    let provider = providers
        .cache_provider(&profile.cache.provider)
        .ok_or_else(|| ClusterError::InvalidConfig {
            reason: format!(
                "profile `{name}`: unknown cache provider `{}`",
                profile.cache.provider
            ),
        })?;
    provider.build_cache(&profile.cache.options).await
}

/// A fluent builder collecting per-profile backend bindings and plugin shutdown
/// hooks. Finish with [`build_and_start`](Self::build_and_start).
#[must_use = "a wiring builder registers nothing until `.build_and_start()` is called"]
pub struct ClusterWiringBuilder {
    hub: Arc<ClientHub>,
    profiles: Vec<(String, ProfileBackends)>,
    stop_hooks: Vec<StopHook>,
    /// Applied to every SDK default backend this builder auto-fills (§5.8.1).
    /// Native backends are passed through untouched and keep their own.
    fence_retention: Duration,
}

impl ClusterWiringBuilder {
    /// Binds `backends` to the typed profile `P`. The marker is passed by value
    /// (mirroring the SDK resolver builders' `profile(marker)`); only
    /// [`ClusterProfile::NAME`] is read — the profile string is never re-typed at
    /// this call site.
    pub fn profile<P: ClusterProfile>(mut self, _marker: P, backends: ProfileBackends) -> Self {
        self.profiles.push((P::NAME.to_owned(), backends));
        self
    }

    /// Binds `backends` to a profile named at runtime — the config-driven path
    /// ([`ClusterWiring::from_config`]) where the profile name comes from operator
    /// YAML rather than a [`ClusterProfile`] marker. The name is validated against
    /// the cluster name rule during [`build_and_start`](Self::build_and_start).
    pub fn profile_named(mut self, name: impl Into<String>, backends: ProfileBackends) -> Self {
        self.profiles.push((name.into(), backends));
        self
    }

    /// Sets how long a lease record outlives the lease it fenced, for every SDK
    /// default backend this builder auto-fills (DESIGN.md).
    ///
    /// [`ClusterWiring::from_config`] calls this with the operator's
    /// `fence_retention`; an embedding library caller that does not gets
    /// [`FENCE_RETENTION_DEFAULT`](cluster_sdk::lease::FENCE_RETENTION_DEFAULT).
    /// A primitive bound to a native backend is unaffected — its fence, if it has
    /// one, is that backend's own business and takes that backend's own option.
    ///
    /// # Errors
    /// [`ClusterError::InvalidConfig`] when `retention` is zero
    /// (`cluster_sdk::lease::validate_fence_retention`).
    pub fn with_fence_retention(mut self, retention: Duration) -> Result<Self, ClusterError> {
        cluster_sdk::lease::validate_fence_retention(retention)?;
        self.fence_retention = retention;
        Ok(self)
    }

    /// Registers a shutdown action — typically a wired plugin handle's `stop()`
    /// future — run once during [`ClusterHandle::stop`] after backends are
    /// deregistered.
    pub fn on_stop<F, Fut>(mut self, hook: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.stop_hooks.push(Box::new(move || Box::pin(hook())));
        self
    }

    /// Resolves every profile's four backends (auto-filling unbound primitives
    /// with the SDK defaults), registers them in the hub under
    /// `cluster:{profile}`, **and makes this process able to resolve them**: the
    /// bound set is published into a fresh [`ProfileRegistry`] and a
    /// [`LocalClusterClient`] over it is registered under `dyn ClusterClient`
    /// (DESIGN.md).
    ///
    /// Resolution happens before any hub mutation, so a failure to build a
    /// default backend cannot leave a partially-registered hub.
    ///
    /// That last half is new with item `K4`, and it is what keeps this method's
    /// promise intact: since `K4` a facade resolves through the process's cluster
    /// client rather than through the per-profile hub scopes, so wiring that
    /// registered only the scopes would bind backends nothing could reach. The
    /// registry is created here because this path *drops* the bound set — the
    /// config-driven [`ClusterWiring::from_config`] hands it to its caller instead,
    /// and the caller (the gear) owns publishing it into the registry the gRPC
    /// services and the readiness check were built over in `init`.
    ///
    /// # Errors
    /// - [`ClusterError::InvalidConfig`] if a default leader-election or lock
    ///   backend is auto-filled over a non-linearizable cache (their consistency
    ///   guard).
    /// - [`ClusterError::InvalidName`] if a profile name violates the cluster
    ///   name rule.
    pub fn build_and_start(self) -> Result<ClusterHandle, ClusterError> {
        let (mut handle, bound) = self.build_and_start_bound()?;
        let profiles = Arc::new(ProfileRegistry::new());
        handle.publish(&profiles, bound);
        // This handle created the registry, so it is the one that clears it -
        // which keeps `stop()` unbinding the profiles on this path.
        handle.profiles = Some(profiles);
        Ok(handle)
    }

    /// Removes and returns every stop hook registered so far.
    ///
    /// Exists for one caller: [`ClusterWiring::from_config`]'s failure path, which
    /// has to shut down the backends already-wired profiles started rather than
    /// drop their hooks unrun (see the note there). Deliberately **not** public —
    /// a builder that has had its hooks taken will not stop what it started, which
    /// is only safe when the caller is about to run them itself.
    fn take_stop_hooks(&mut self) -> Vec<StopHook> {
        std::mem::take(&mut self.stop_hooks)
    }

    /// [`build_and_start`](Self::build_and_start), also returning the
    /// bound-profile set (DESIGN §5.2).
    ///
    /// The config-driven [`ClusterWiring::from_config`] surfaces this set to its
    /// caller; the programmatic builder path does not need it yet, so
    /// `build_and_start` keeps its shape and drops it.
    ///
    /// A direct builder caller registered its stop hooks itself and still owns
    /// whatever they close over, so a resolution failure drops them — the
    /// pre-existing contract. Only [`ClusterWiring::from_config`], which built
    /// those backends on the caller's behalf, uses the hook-returning form so it
    /// can shut them down.
    fn build_and_start_bound(self) -> Result<WiredCluster, ClusterError> {
        self.build_and_start_bound_returning_hooks()
            .map_err(|(err, hooks)| {
                drop(hooks);
                err
            })
    }

    /// [`build_and_start_bound`](Self::build_and_start_bound), handing the
    /// accumulated stop hooks back on failure instead of dropping them.
    ///
    /// The distinction matters only for [`ClusterWiring::from_config`], which
    /// starts every backend itself: a resolution failure there leaves real pools,
    /// tasks, and connections running behind these hooks, and the Redis plugin's
    /// ADR-006 `Drop` guard turns leaking them into a debug-build panic
    /// (redis-cluster-plugin/docs/DESIGN.md §11).
    fn build_and_start_bound_returning_hooks(
        mut self,
    ) -> Result<WiredCluster, (ClusterError, Vec<StopHook>)> {
        macro_rules! unwind_on_err {
            ($result:expr, $self:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(err) => return Err((err, $self.take_stop_hooks())),
                }
            };
        }

        // Phase 1 — resolve all backends (fallible) before touching the hub.
        // Default leader-election and lock backends the wiring itself creates
        // expose a shutdown-revoke seam; collect them so
        // `ClusterHandle::stop` can revoke in-flight coordination before shutdown
        // completes (DESIGN §3.13). Native (explicitly-bound) backends are not
        // revoked here — they manage shutdown through their own plugin stop hook.
        let profiles = std::mem::take(&mut self.profiles);
        let mut bound = Vec::with_capacity(profiles.len());
        let mut revokers: Vec<Arc<dyn ShutdownRevoke>> = Vec::new();
        for (name, backends) in profiles {
            bound.push(unwind_on_err!(
                resolve_profile_backends(name, backends, self.fence_retention, &mut revokers),
                self
            ));
        }

        // Phase 2 — register every primitive under the profile scope. A failure
        // partway (e.g. a later profile with an invalid name) must not leave
        // earlier profiles half-registered, so roll back everything registered
        // so far before propagating the error — the hub stays all-or-nothing.
        let mut registered: Vec<String> = Vec::with_capacity(bound.len());
        for profile in &bound {
            unwind_on_err!(
                register_profile_or_rollback(&self.hub, profile, &registered),
                self
            );
            registered.push(profile.name.clone());
        }

        let stop_hooks = self.take_stop_hooks();
        Ok((
            ClusterHandle {
                hub: self.hub,
                registered,
                stop_hooks,
                revokers,
                profiles: None,
                stopped: false,
            },
            bound,
        ))
    }
}

/// Fills any primitive `backends` left unbound with its SDK default over a
/// **reserved view** of `backends.cache`, collecting each default's
/// shutdown-revoke seam into `revokers` (DESIGN §3.13). Explicitly-bound
/// (native) primitives are passed through untouched.
///
/// The two cache handles are not interchangeable and the split is the point.
/// [`BoundProfile::cache`](crate::BoundProfile) stays the *raw* handle — it is
/// what `CacheService` and the health probe serve — while the defaults'
/// [lease store](cluster_sdk::reserved_lease_cache) sits in a keyspace no cache
/// request can name. The descriptor projection and provider identity below
/// deliberately read the raw handle: they report what the *backend* is, which
/// scoping does not change.
///
/// The resolved backends are then described: consistency and features are read
/// off the real backends, provider identity comes from config where there is
/// config, and each primitive's instance is identified (DESIGN §5.2).
fn resolve_profile_backends(
    name: String,
    backends: ProfileBackends,
    fence_retention: Duration,
    revokers: &mut Vec<Arc<dyn ShutdownRevoke>>,
) -> Result<Arc<BoundProfile>, ClusterError> {
    let cache = backends.cache;
    // The two cache-backed defaults get a *reserved* view of the cache, never the
    // handle the cache API serves. Both keyspaces are physically one store, so
    // sharing the handle made the lease records reachable through
    // `CacheService`: `get("lock/x")` returned a decodable `LeaseRecord`, `put`
    // installed one held by nobody, `delete` reset the row so the fence restarted
    // and a stale token matched again — mutual exclusion forgeable by any caller
    // with cache access, authenticated or not. `reserved_lease_cache` prefixes the
    // lease keys with a prefix no legal cache key can express, and `CacheService`
    // refuses the sigil at its boundary (`api::grpc::cache`).
    //
    // Built once, above both branches, and deliberately not conditional on either
    // default being needed: an `Arc` allocation on the wiring path is not worth a
    // second construction site for the one invariant this whole change rests on.
    let lease_cache = cluster_sdk::reserved_lease_cache(Arc::clone(&cache));
    let leader_election: Arc<dyn LeaderElectionBackend> =
        if let Some(backend) = backends.leader_election {
            backend
        } else {
            // Both arms build the same backend over the reserved lease cache; they
            // differ only in whether the consistency guard is enforced or explicitly
            // waived (`ProfileBackends::allow_weak_leader_election`, reached from YAML
            // as `leader_election: { provider: default, allow_weak_consistency: true }`).
            // The weak constructor is infallible and logs its own split-brain
            // warning, so the `?` lives on the guarded arm alone.
            let backend = if backends.allow_weak_leader_election {
                CasBasedLeaderElectionBackend::new_allow_weak_consistency(Arc::clone(&lease_cache))
            } else {
                CasBasedLeaderElectionBackend::new(Arc::clone(&lease_cache))?
            };
            let default = Arc::new(backend.with_fence_retention(fence_retention));
            revokers.push(Arc::clone(&default) as Arc<dyn ShutdownRevoke + Send + Sync>);
            default as Arc<dyn LeaderElectionBackend>
        };
    let lock: Arc<dyn DistributedLockBackend> = if let Some(backend) = backends.lock {
        backend
    } else {
        // The lock default shares the leader default's constructor guard, so a
        // weak-cache profile has to waive both or neither — waiving only the leader
        // one moves the startup failure four lines down rather than resolving it.
        let backend = if backends.allow_weak_lock {
            CasBasedDistributedLockBackend::new_allow_weak_consistency(Arc::clone(&lease_cache))
        } else {
            CasBasedDistributedLockBackend::new(Arc::clone(&lease_cache))?
        };
        let default = Arc::new(backend.with_fence_retention(fence_retention));
        revokers.push(Arc::clone(&default) as Arc<dyn ShutdownRevoke>);
        default as Arc<dyn DistributedLockBackend>
    };
    let providers = backends
        .providers
        .unwrap_or_else(|| ProviderIdentities::from_backends(&cache, &leader_election, &lock));
    let descriptor = describe_profile(&name, &cache, &leader_election, &lock, &providers);
    let instances = ProfileInstanceRefs {
        cache: InstanceId::of(&cache),
        leader_election: InstanceId::of(&leader_election),
        lock: InstanceId::of(&lock),
    };
    Ok(Arc::new(BoundProfile::new(
        name,
        cache,
        leader_election,
        lock,
        descriptor,
        instances,
    )))
}

/// The profile's [`ProfileDescriptor`]: what each bound backend *declares*,
/// which is the one piece of profile knowledge a sync accessor on a remote
/// backend cannot fetch for itself (DESIGN §5.5).
///
/// Consistency and features are read from the real backends rather than from
/// config, so a descriptor cannot claim a capability the backend does not
/// declare.
fn describe_profile(
    name: &str,
    cache: &Arc<dyn ClusterCacheBackend>,
    leader_election: &Arc<dyn LeaderElectionBackend>,
    lock: &Arc<dyn DistributedLockBackend>,
    providers: &ProviderIdentities,
) -> ProfileDescriptor {
    ProfileDescriptor {
        name: name.to_owned(),
        cache: CacheDescriptor {
            consistency: cache.consistency().into(),
            features: cache.features().into(),
            provider: providers.cache.clone(),
        },
        lock: LockDescriptor {
            features: lock.features().into(),
            provider: providers.lock.clone(),
        },
        leader_election: LeaderElectionDescriptor {
            features: leader_election.features().into(),
            provider: providers.leader_election.clone(),
        },
        health: WIRED_HEALTH,
    }
}

/// Registers `profile`'s three primitives in `hub`. On failure, deregisters
/// `profile` itself and every name in `registered` so the hub stays
/// all-or-nothing, logs a warning naming the failed profile and rollback
/// count, and returns the error. On success, logs registration; the caller adds
/// the profile's name to `registered`.
///
/// The backend `Arc`s are cloned into the hub rather than moved, because the
/// bound-profile set keeps its own strong references (DESIGN §5.3). The hub
/// receives the same instances it always did.
fn register_profile_or_rollback(
    hub: &Arc<ClientHub>,
    profile: &BoundProfile,
    registered: &[String],
) -> Result<(), ClusterError> {
    let result = (|| {
        register_cache_backend(hub, &profile.name, Arc::clone(&profile.cache))?;
        register_leader_election_backend(hub, &profile.name, Arc::clone(&profile.leader_election))?;
        register_lock_backend(hub, &profile.name, Arc::clone(&profile.lock))
    })();
    let Err(err) = result else {
        tracing::info!(profile = %profile.name, "cluster profile registered");
        return Ok(());
    };
    tracing::warn!(
        profile = %profile.name,
        error = %err,
        rolled_back = registered.len(),
        "cluster profile registration failed; rolling back all registered profiles"
    );
    // Unwind the just-attempted profile and every prior one. Any primitive of
    // `profile.name` that did register is removed too; deregister of an
    // unregistered name is a harmless no-op.
    deregister_profile(hub, &profile.name);
    for name in registered {
        deregister_profile(hub, name);
    }
    Err(err)
}

/// The running cluster wiring. Backends are registered in the hub; consumers
/// resolve them with the SDK resolvers (e.g.
/// `ClusterCacheV1::resolver(handle.hub())`). Owns the wired plugins' shutdown.
pub struct ClusterHandle {
    hub: Arc<ClientHub>,
    registered: Vec<String>,
    stop_hooks: Vec<StopHook>,
    /// Shutdown-revoke seams for the wiring-created default leader-election and
    /// lock backends, revoked first on [`stop`](ClusterHandle::stop).
    revokers: Vec<Arc<dyn ShutdownRevoke>>,
    /// The registry this handle **created** and therefore clears on
    /// [`stop`](ClusterHandle::stop) — the programmatic
    /// [`build_and_start`](ClusterWiringBuilder::build_and_start) path only.
    /// `None` on the gear's path, where the gear owns its registry and clears it
    /// itself (§4.8 phase 4), and `None` for any other caller of
    /// [`publish`](ClusterHandle::publish).
    profiles: Option<Arc<ProfileRegistry>>,
    /// Set by [`stop`](ClusterHandle::stop) so the [`Drop`] guard can tell a
    /// graceful shutdown apart from a forgotten one (ADR-006 §Confirmation).
    stopped: bool,
}

impl ClusterHandle {
    /// The hub the backends are registered in, for consumers to resolve against.
    #[must_use]
    pub fn hub(&self) -> &Arc<ClientHub> {
        &self.hub
    }

    /// Publishes `bound` into `profiles` and registers a [`LocalClusterClient`]
    /// over it under `dyn ClusterClient` — the two steps that make a wired process
    /// able to resolve (DESIGN.md).
    ///
    /// Publishing comes first, so a consumer that finds the client finds a
    /// populated registry behind it. The reverse order is not broken — a request
    /// landing between the two is answered `ProfileNotBound`, which the
    /// whole pre-`start` window already answers — but it would be a needless
    /// window.
    ///
    /// Registration is last-write-wins and this deliberately does not remove it at
    /// [`stop`](ClusterHandle::stop): clearing the registry already makes every
    /// method answer `ProfileNotBound`, and it does so *naming the profile*, where
    /// an absent client would leave a resolver reporting only the coarser "nothing
    /// is wired in this process" (§4.9.1).
    ///
    /// **Clearing the registry at shutdown stays with whoever owns it**: the gear
    /// clears its own in `stop`, because that registry outlives any single wiring
    /// and must be cleared even when `start` failed before a handle existed. Only
    /// [`build_and_start`](ClusterWiringBuilder::build_and_start), which creates a
    /// registry precisely because nothing else will, hands that job to the handle.
    pub fn publish(&mut self, profiles: &Arc<ProfileRegistry>, bound: Vec<Arc<BoundProfile>>) {
        profiles.publish(bound);
        tracing::debug!(
            generation = profiles.generation(),
            "cluster profile registry published"
        );
        self.hub
            .register::<dyn ClusterClient>(Arc::new(LocalClusterClient::new(Arc::clone(profiles))));
    }

    /// The single shutdown entry point (DESIGN §3.7, §3.13).
    ///
    /// 1. **Revoke in-flight coordination first** (`cpt-cf-clst-fr-shutdown-revoke`):
    ///    every wiring-created default backend is revoked — an active leader
    ///    observes `Status(Lost)` then `Closed(Shutdown)` and an in-flight
    ///    blocking `lock()` waiter returns `Err(Shutdown)` — before this returns,
    ///    so no consumer can resume believing it still holds coordination state.
    /// 2. Deregister every registered backend — so later resolves report
    ///    [`ClusterError::ProfileNotBound`].
    /// 3. Run the plugin shutdown hooks in reverse-start order (DESIGN §3.7: last
    ///    started is stopped first). The standalone plugin's stop hook closes
    ///    active **cache** watches via the plugin's `StandaloneCache::shutdown`,
    ///    so a cache-watch consumer observes `Closed(Shutdown)` one phase after the
    ///    leader/lock revocation — still within `stop()` (the chosen simplest
    ///    path; the slight ordering is intentional).
    ///
    /// No best-effort remote cleanup is attempted; TTL bounds any remaining
    /// cluster resources — held leader claims and locks
    /// all lapse via their backend TTL (`cpt-cf-clst-fr-shutdown-ttl-cleanup`).
    pub async fn stop(mut self) {
        tracing::info!(
            profiles = self.registered.len(),
            "stopping cluster wiring: revoking in-flight coordination"
        );
        for revoker in &self.revokers {
            revoker.revoke().await;
        }
        // Clear the published set before the hub scopes, mirroring the gear's own
        // stop order: a request arriving during the drain resolves to
        // `ProfileNotBound` rather than reaching a backend about to be torn down
        // (DESIGN.md phase C). The `dyn ClusterClient`
        // registration itself stays - see `publish`.
        if let Some(profiles) = &self.profiles {
            profiles.clear();
        }
        deregister_all(&self.hub, &self.registered);
        // `mem::take` rather than `into_iter` because `ClusterHandle` now owns a
        // `Drop` impl, and you cannot move a field out of a type that implements
        // `Drop`. Draining the hooks in place leaves an empty `Vec` behind.
        for hook in std::mem::take(&mut self.stop_hooks).into_iter().rev() {
            hook().await;
        }
        // Graceful shutdown completed — tell the `Drop` guard not to fire.
        self.stopped = true;
        tracing::info!("cluster wiring stopped");
    }
}

/// Deregisters every profile in `names`, logging each at `debug` (DESIGN §3.7).
fn deregister_all(hub: &Arc<ClientHub>, names: &[String]) {
    for name in names {
        tracing::debug!(profile = %name, "deregistering cluster profile");
        deregister_profile(hub, name);
    }
}

/// Diagnostic guard (ADR-006 §Confirmation): a [`ClusterHandle`] must be released
/// through [`stop`](ClusterHandle::stop). Dropping one without stopping leaks the
/// wired plugins' background tasks (cache TTL sweepers, leader-renewal loops), so
/// surface the bug loudly rather than silently — a debug-build panic, a
/// release-build warn-log. The [`std::thread::panicking`] guard skips the debug
/// panic during unwind so a forgotten handle dropped *while already panicking*
/// degrades to a warning instead of a double-panic process abort (ADR-002).
impl Drop for ClusterHandle {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        if std::thread::panicking() {
            tracing::warn!(
                "ClusterHandle dropped during panic unwind without stop(); \
                 skipping debug panic to avoid double-panic abort"
            );
            return;
        }
        #[cfg(debug_assertions)]
        panic!("ClusterHandle dropped without stop() - programming error");
        #[cfg(not(debug_assertions))]
        tracing::warn!(
            "ClusterHandle dropped without stop() - programming error; \
             background tasks may leak"
        );
    }
}

/// Deregisters all three primitives bound under `cluster:{name}`. Deregistration
/// only fails on an invalid name, which cannot occur for a name that registered
/// successfully, and deregistering an unbound primitive is a harmless no-op — so
/// the presence reports are discarded.
fn deregister_profile(hub: &Arc<ClientHub>, name: &str) {
    deregister_cache_backend(hub, name).ok();
    deregister_leader_election_backend(hub, name).ok();
    deregister_lock_backend(hub, name).ok();
}

#[cfg(test)]
#[path = "wiring_tests.rs"]
mod wiring_tests;
