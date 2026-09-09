//! The `cluster` gear — the `RunnableCapability` that owns the [`ClusterHandle`]
//! across its lifecycle (DESIGN §3.7, as amended: the wiring library and the host
//! gear are the same crate, matching the platform's one-gear-per-domain layout).
//!
//! `init` captures the hub and parses [`ClusterConfig`]; `start` assembles the
//! provider registry, calls [`ClusterWiring::from_config`], and takes ownership of
//! the resulting [`ClusterHandle`]; `stop` runs [`ClusterHandle::stop`] under the
//! framework's shutdown deadline. The builder/handle and config types remain `pub`
//! library surface (see crate root) so consumers may embed the wiring directly.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use anyhow::anyhow;
use async_trait::async_trait;
use axum::Router;
use tokio_util::sync::CancellationToken;
use tonic::server::NamedService;
use toolkit::client_hub::ClientHub;
use toolkit::contracts::{
    GrpcServiceCapability, OpenApiRegistry, RegisterGrpcServiceFn, RestApiCapability,
    RunnableCapability,
};
use toolkit::{Gear, GearCtx, Healthcheck};

use crate::api::grpc::{
    CacheService, ClusterProfileService, DistributedLockService, ElectionSubscriptions,
    LeaderElectionService, ServiceContext,
};
use crate::config::ClusterConfig;
use crate::domain::health::ClusterReadiness;
use crate::domain::local_client::LocalClusterClient;
use crate::domain::provider::ProviderRegistry;
use crate::domain::registry::ProfileRegistry;
use crate::domain::wiring::{ClusterHandle, ClusterWiring};
use cluster_sdk::grpc::stubs;

#[toolkit::gear(name = "cluster", capabilities = [stateful, system, grpc, rest])]
struct ClusterGear {
    /// Captured in `init` so `start` (which gets no `GearCtx`) can register
    /// backends into it.
    hub: OnceLock<Arc<ClientHub>>,
    /// Parsed operator config, captured in `init` and consumed in `start`.
    config: OnceLock<ClusterConfig>,
    /// The profile index: created **empty** in `init` and populated by `start`
    /// (DESIGN §5.2).
    ///
    /// The split matters. Services and the healthcheck are collected before
    /// `start` runs, so they must capture this registry rather than a backend
    /// (§4.2's lifecycle constraint) — and a request arriving in the window
    /// between the two resolves to `ProfileNotBound`, which is the correct answer
    /// and needs no new error variant (invariant I3).
    profiles: OnceLock<Arc<ProfileRegistry>>,
    /// The election subscription table — the only server-side coordination state
    /// (§5.4).
    ///
    /// Created in `init` for the same reason as `profiles`: the leader service is
    /// built in the gRPC registration phase, and `stop` needs the *same* table to
    /// fan terminal events out over (`S5`). It is explicitly **not** a lease —
    /// nothing in the lease path reads it (invariant I7).
    subscriptions: OnceLock<Arc<ElectionSubscriptions>>,
    /// Parent of the abandoned-subscription sweep tokens (`S2`, §5.4.1).
    ///
    /// A **factory**, not the token the running sweep observes: each `start`
    /// mints a fresh `child_token()` (stored in `sweep_task`) so a stop→start
    /// cycle gets a live token. Cancelling this parent would cancel every future
    /// child too, which is exactly the bug a single once-created token had — a
    /// second `start` inheriting an already-cancelled token whose sweep breaks on
    /// the first tick. Its own token rather than `start`'s: that one is the
    /// framework's start-phase signal, and the sweep must run for the whole
    /// serving life, not until the phase ends.
    sweep: CancellationToken,
    /// The running sweep's cancellation token and join handle, owned from `start`
    /// to `stop`.
    ///
    /// Stored rather than discarded so `stop` can cancel the *current* sweep and
    /// **join** it — a background task that outlives the gear that spawned it is
    /// how a test suite ends up with one per case, and a dropped handle is never
    /// awaited.
    sweep_task: Mutex<Option<(CancellationToken, tokio::task::JoinHandle<()>)>>,
    /// The running wiring, owned from `start` to `stop`.
    handle: Mutex<Option<ClusterHandle>>,
}

impl Default for ClusterGear {
    fn default() -> Self {
        Self {
            hub: OnceLock::new(),
            config: OnceLock::new(),
            profiles: OnceLock::new(),
            subscriptions: OnceLock::new(),
            sweep: CancellationToken::new(),
            sweep_task: Mutex::new(None),
            handle: Mutex::new(None),
        }
    }
}

impl ClusterGear {
    /// Assembles the provider registry from the backend plugins linked into this
    /// build; future plugins add a `with_*_provider` line here.
    ///
    /// The two Redis lines are what make `provider: redis` resolvable at all.
    /// They are separate registrations rather than one, because the SDK keeps the
    /// cache and lock provider traits independent: a profile may bind
    /// `lock: { provider: redis }` over a cache on another backend entirely
    /// (redis-cluster-plugin/docs/DESIGN.md §3.5), and the standalone
    /// `RedisLockPlugin` behind `RedisLockProvider` opens its own pool for
    /// exactly that shape.
    ///
    /// Note what is deliberately *not* here: no Redis leader-election provider.
    /// It is the SDK default over the Redis cache
    /// (§7), and since that cache normally declares `EventuallyConsistent`, a
    /// profile binding `cache: { provider: redis }` and omitting
    /// `leader_election` **fails startup** until the operator either routes
    /// leader election elsewhere or opts in via `provider: default` plus
    /// `allow_weak_consistency` (Phase 1's [`DEFAULT_PROVIDER`] sentinel). That
    /// is the honest declaration doing its job, not a missing registration.
    ///
    /// [`DEFAULT_PROVIDER`]: crate::DEFAULT_PROVIDER
    fn provider_registry() -> ProviderRegistry {
        ProviderRegistry::new()
            .with_cache_provider(Arc::new(standalone_cluster_plugin::StandaloneCacheProvider))
            .with_cache_provider(Arc::new(postgres_cluster_plugin::PostgresCacheProvider))
            .with_lock_provider(Arc::new(postgres_cluster_plugin::PostgresLockProvider))
            .with_cache_provider(Arc::new(redis_cluster_plugin::RedisCacheProvider))
            .with_lock_provider(Arc::new(redis_cluster_plugin::RedisLockProvider))
    }

    /// The profile index, or an error naming the lifecycle violation.
    ///
    /// Every caller runs after `init` in the framework's phase order, so the error
    /// arm is unreachable in a correct host — it exists so a wrong order is a loud
    /// failure rather than a silently service-less gear.
    fn profiles(&self) -> anyhow::Result<Arc<ProfileRegistry>> {
        self.profiles.get().map(Arc::clone).ok_or_else(|| {
            anyhow!(
                "{}: profile registry not created - init must run first",
                Self::MODULE_NAME
            )
        })
    }

    /// The election subscription table, on the same terms as
    /// [`profiles`](Self::profiles).
    fn subscriptions(&self) -> anyhow::Result<Arc<ElectionSubscriptions>> {
        self.subscriptions.get().map(Arc::clone).ok_or_else(|| {
            anyhow!(
                "{}: subscription table not created - init must run first",
                Self::MODULE_NAME
            )
        })
    }
}

/// `server`'s wire service name, read from **tonic's own** `NamedService::NAME`.
///
/// Taken from the generated server rather than a hand-written constant so the
/// registered name cannot drift from the `.proto`'s `package.Service` — the string
/// routing actually keys on (§6.11).
///
/// Inferred from a reference rather than spelled as a turbofish because the four
/// generated server types are long enough that naming each twice is where a
/// copy-paste error would hide.
fn service_name<S: NamedService>(_server: &S) -> &'static str {
    S::NAME
}

#[async_trait]
impl Gear for ClusterGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: ClusterConfig = ctx.config_or_default()?;
        self.hub
            .set(ctx.client_hub())
            .map_err(|_| anyhow!("{} already initialized", Self::MODULE_NAME))?;
        self.config
            .set(config)
            .map_err(|_| anyhow!("{} already initialized", Self::MODULE_NAME))?;
        // Created here, empty, because the gear's services and healthcheck are
        // collected before `start` populates it (DESIGN §4.2, §5.2).
        self.profiles
            .set(Arc::new(ProfileRegistry::new()))
            .map_err(|_| anyhow!("{} already initialized", Self::MODULE_NAME))?;
        self.subscriptions
            .set(Arc::new(ElectionSubscriptions::new()))
            .map_err(|_| anyhow!("{} already initialized", Self::MODULE_NAME))?;

        // Claim `dyn ClusterClient` for this process **now**, in `init`, over the
        // empty registry created above -- one phase before the framework's
        // proxy-wiring phase, and therefore before cluster-sdk's
        // `ConsumerRegistration` is replayed (`init` -> wiring -> post_init,
        // `host_runtime.rs:609-613`).
        //
        // That ordering is the whole point, and without it a process that *hosts*
        // cluster wires a remote client to its own socket: at wiring time the hub
        // would be empty (the local client used to arrive only with `start`), so
        // the registration would derive an endpoint, build a channel, and mark the
        // gear a remote readiness dependency of itself. Worse, it registers through
        // `register_remote_proxy`, which records the type as a proxy permanently --
        // so the local client `start` registers afterwards would be invisible to
        // every later `try_get_local`, and `R5`'s local-wins identity would be
        // false in exactly the deployment where it matters most.
        //
        // Registering early is safe because `LocalClusterClient` holds the
        // *registry*, not a snapshot of it: until `start` publishes, every method
        // answers `ProfileNotBound`, which is the correct answer for that window
        // and needs no new error variant (invariant I3). `start`'s `publish` then
        // re-registers the same client over the same registry -- last-write-wins on
        // an equal value, so it is a no-op that keeps the programmatic
        // `build_and_start` path (which has no `init`) unchanged.
        let profiles = self.profiles()?;
        ctx.client_hub()
            .register::<dyn cluster_sdk::ClusterClient>(Arc::new(LocalClusterClient::new(
                profiles,
            )));
        Ok(())
    }
}

/// Platform tier, so cluster initialises in the system phase ahead of application
/// gears (§4.2).
///
/// Empty on purpose, and the emptiness is the whole implementation: every
/// `SystemCapability` method is defaulted, so the capability is the ordering flag
/// §4.2 describes and nothing more. Cluster wants neither hook — `pre_init` runs
/// before *any* gear's `init`, and `post_init` before the REST and gRPC phases, so
/// neither is a place backends could exist. The same shape as `authn-resolver`,
/// `tenant-resolver`, `authz-resolver` and `credstore`.
///
/// It is still a trait that must be implemented, which §4.2's table does not say:
/// the `#[toolkit::gear]` macro emits no *assertion* for `system` but its
/// registration casts to `Arc<dyn SystemCapability>`, so omitting this is a
/// build error from the macro rather than the documented no-op.
impl toolkit::contracts::SystemCapability for ClusterGear {}

#[async_trait]
impl GrpcServiceCapability for ClusterGear {
    /// The four coordination services, collected in the gRPC registration phase
    /// (§4.2).
    ///
    /// **Nothing here captures a backend**, and it cannot: this phase runs before
    /// `RunnableCapability::start`, where the wiring builds them. Every service is
    /// built from a [`ServiceContext`] over the `Arc<ProfileRegistry>`, so a
    /// request arriving in the window between registration and `start` resolves to
    /// `ProfileNotBound` rather than reaching a half-built backend (§5.2,
    /// invariant I3).
    async fn get_grpc_services(
        &self,
        _ctx: &GearCtx,
    ) -> anyhow::Result<Vec<RegisterGrpcServiceFn>> {
        // Cluster does **not** authenticate in-handler. The platform-plane check
        // is `grpc-hub`'s `InternalAuthGrpcLayer`, which wraps every service this
        // process serves: it validates `x-toolkit-internal-token` and stamps a
        // `PlatformSecurityContext` into the request extensions, which
        // `CallerResolver::resolve` then reads (§4.6).
        //
        // The reason it lives there and not here is that the credential belongs to
        // the *process*, not to this gear. `grpc-hub` owns it once for every gear
        // it hosts, configured at `gears.grpc-hub.config.internal_auth`; a second
        // authenticator wired into cluster would be the same `InternalAuthConfig`
        // in a second place, settable to a different value, with nothing
        // reconciling the two. That is the failure mode this avoids.
        //
        // Enforcement is therefore an opt-in `grpc-hub` config flip, not a code
        // change here: with `internal_auth` unset the layer stamps nothing, every
        // caller resolves to `UNAUTHENTICATED_CALLER`, and the `NetworkPolicy` on
        // the coordination port is the only boundary -- the pre-retrofit
        // trusted-network behaviour (`A1`), stated in the operator guidance. Turning
        // it on changes nothing above `CallerResolver::resolve`.
        let ctx = ServiceContext::new(self.profiles()?);
        let subscriptions = self.subscriptions()?;

        let cache = stubs::cache::cluster_cache_api_server::ClusterCacheApiServer::new(
            CacheService::new(ctx.clone()),
        );
        let lock = stubs::lock::distributed_lock_api_server::DistributedLockApiServer::new(
            DistributedLockService::new(ctx.clone()),
        );
        let leader = stubs::leader::leader_election_api_server::LeaderElectionApiServer::new(
            LeaderElectionService::new(ctx.clone(), subscriptions),
        );
        let profile = stubs::profile::cluster_profile_api_server::ClusterProfileApiServer::new(
            ClusterProfileService::new(ctx),
        );

        // Each installer is an `Fn`, so it may run more than once and clones per
        // call. Every generated `*Server<T>` is `Clone` whatever `T` is (its inner
        // is an `Arc`) and all four services are `Clone`, so a clone is a refcount.
        Ok(vec![
            RegisterGrpcServiceFn {
                service_name: service_name(&cache),
                register: Box::new(move |routes| {
                    routes.add_service(cache.clone());
                }),
            },
            RegisterGrpcServiceFn {
                service_name: service_name(&lock),
                register: Box::new(move |routes| {
                    routes.add_service(lock.clone());
                }),
            },
            RegisterGrpcServiceFn {
                service_name: service_name(&leader),
                register: Box::new(move |routes| {
                    routes.add_service(leader.clone());
                }),
            },
            RegisterGrpcServiceFn {
                service_name: service_name(&profile),
                register: Box::new(move |routes| {
                    routes.add_service(profile.clone());
                }),
            },
        ])
    }
}

impl RestApiCapability for ClusterGear {
    /// No routes yet, and **never a primitive**: the coordination data plane is
    /// gRPC only (§2.2). `S4` adds the admin/diagnostic routes here, built
    /// `.authenticated()` and without `.exposed()`.
    fn register_rest(
        &self,
        _ctx: &GearCtx,
        router: Router,
        _openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<Router> {
        Ok(router)
    }

    /// The composite readiness check (§4.4).
    ///
    /// Like the services, it captures the **registry** and not a backend — it is
    /// collected in the REST phase, two phases before any backend exists.
    ///
    /// Returning `None` would opt cluster out of readiness altogether and report
    /// the pod ready with nothing bound, so the unreachable no-`init` arm still
    /// yields a check: one over a fresh empty registry, which is permanently at
    /// generation 0 and therefore reports `Starting`. That is the fail-safe
    /// direction, and the `error!` says why it happened.
    fn healthcheck(&self, _ctx: &GearCtx) -> Option<Arc<dyn Healthcheck>> {
        let profiles = self.profiles.get().map_or_else(
            || {
                tracing::error!(
                    "{}: healthcheck collected before init - reporting Starting until it runs",
                    Self::MODULE_NAME
                );
                Arc::new(ProfileRegistry::new())
            },
            Arc::clone,
        );
        // The configured names are what let the check say a profile is *missing*
        // rather than merely absent (§4.4's `Ready` row is over every configured
        // profile). An unparsed config means none are expected yet, which pairs
        // with the generation-0 verdict above.
        let configured = self
            .config
            .get()
            .map(|config| config.profiles.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        Some(Arc::new(ClusterReadiness::new(profiles, configured)))
    }
}

#[async_trait]
impl RunnableCapability for ClusterGear {
    async fn start(&self, _cancel: CancellationToken) -> anyhow::Result<()> {
        let hub = self.hub.get().ok_or_else(|| {
            anyhow!(
                "{}: hub not set — init must run before start",
                Self::MODULE_NAME
            )
        })?;
        let config = self.config.get().ok_or_else(|| {
            anyhow!(
                "{}: config not set — init must run before start",
                Self::MODULE_NAME
            )
        })?;

        let profiles = self.profiles.get().ok_or_else(|| {
            anyhow!(
                "{}: profile registry not created — init must run before start",
                Self::MODULE_NAME
            )
        })?;

        // Backends (and their background tasks) come up here and are registered
        // under `cluster:{profile}`; the handle owns each plugin's shutdown hook.
        let (mut handle, bound) =
            ClusterWiring::from_config(Arc::clone(hub), config, &Self::provider_registry()).await?;

        // Publishing is what makes the profiles addressable by name: requests that
        // arrived before this point were answered `ProfileNotBound` (DESIGN §5.2).
        // It happens after the wiring succeeds, so a failed `start` publishes
        // nothing and the registry stays empty rather than half-populated — the
        // same all-or-nothing property the hub registration has.
        //
        // The same call registers Profile 1's half of the process seam (DESIGN
        // §3.1, §11.2): consumers in *this* process resolve their backends through
        // `dyn ClusterClient`, and this is the implementation they find — so no
        // remote client is ever built here.
        //
        // Unconditional, and the gear knows nothing about which deployment profile
        // it is in. In Profile 3 it is alone in its pod, nothing resolves against
        // it locally, and the registration is inert rather than wrong; whether any
        // consumer finds it is a property of what the binary linked.
        //
        // The registry passed in is the gear's own — the one `init` created and the
        // gRPC services and readiness check were built over — rather than one the
        // wiring makes for itself, so there is exactly one published profile set in
        // this process.
        handle.publish(profiles, bound);

        // The abandoned-subscription sweep (`S2`, §5.4.1). Here rather than in
        // `init` because a background task belongs to the running gear, and here
        // rather than in the wiring because the table is the *gear's*: it is
        // shared with the gRPC services, and the programmatic
        // `build_and_start` path serves no gRPC and has none.
        //
        // It is not gated on there being any subscriptions. A sweep over an empty
        // table is a lock and a walk of nothing, and the gauge it publishes is
        // what says "zero" rather than nothing at all.
        //
        // A fresh child token per `start`, kept alongside its handle so `stop`
        // can cancel *this* sweep and join it. Reusing one token created in
        // `Default` would hand a second `start` an already-cancelled token,
        // breaking its sweep on the first tick.
        let sweep_token = self.sweep.child_token();
        let sweeping = crate::api::grpc::spawn_subscription_sweep(
            self.subscriptions()?,
            crate::api::grpc::SWEEP_INTERVAL,
            crate::api::grpc::SubscriptionMetrics::global(),
            sweep_token.clone(),
        );
        *self
            .sweep_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((sweep_token, sweeping));

        *self.handle.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
        Ok(())
    }

    async fn stop(&self, deadline: CancellationToken) -> anyhow::Result<()> {
        // End the sweep before anything else. It reads only the subscription
        // table, so leaving it running through the drain would cost nothing —
        // but a background task that outlives the gear that spawned it is how a
        // test suite ends up with one per case. Cancel *this* start's token
        // (never the parent, or every future child is born cancelled) and join
        // the handle so the task is truly gone rather than merely signalled; the
        // sweep breaks out of its sleep on cancel, so the join returns promptly,
        // and the deadline bounds it against a wedged task all the same.
        let sweep_task = self
            .sweep_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((sweep_token, sweeping)) = sweep_task {
            sweep_token.cancel();
            tokio::select! {
                _ = sweeping => {}
                () = deadline.cancelled() => {}
            }
        }

        // §4.8's terminal two-step for Profile 3, and item `S5`'s whole job. This
        // is the only caller `ElectionSubscriptions::broadcast_terminal` has, and
        // before it existed the doc table at `api/grpc/leader.rs` described events
        // no subscriber could ever receive: `stop` cancelled the sweep and dropped
        // the wiring without touching the subscription table at all.
        //
        // `Status(Lost)` then `Closed(Shutdown)`, in that order, because that is
        // the sequence a Profile 1 consumer sees from
        // `LeaderWatchSender::revoke_for_shutdown` and invariant I1 requires the
        // two profiles to speak one vocabulary. Both travel through the
        // `TERMINAL_HEADROOM` slots reserved per reader at `attach`, so neither can
        // be dropped by a full buffer and neither can block this drain — a
        // subscriber that stopped reading must not be able to hold shutdown open.
        //
        // **Unconditional `Status(Lost)`.** The table records subscriptions, not
        // leadership (§5.4), so it cannot tell a leader from a follower — and a
        // follower being told to treat its leadership as lost is a no-op, where a
        // leader not being told is the failure the two-step exists to prevent.
        // `revoke_for_shutdown(was_leader)` can be selective because the election
        // task knows the answer; this cannot, and fails safe instead.
        //
        // **Before the profile registry is cleared, and before `handle.stop()`.**
        // Not for the registry's sake — this reads only the subscription table — but
        // because `handle.stop()` is where the awaiting starts: the streams have to
        // be ending while the drain waits, or the wait is for something that has
        // not been asked to finish. Clearing the snapshot afterwards keeps the
        // "requests during the drain get `ProfileNotBound`" property below intact,
        // since these two calls await nothing and open no window.
        if let Some(subscriptions) = self.subscriptions.get() {
            use cluster_sdk::leader::{LeaderStatus, LeaderWatchEvent};

            subscriptions.broadcast_terminal(&LeaderWatchEvent::Status(LeaderStatus::Lost));
            subscriptions.broadcast_terminal(&LeaderWatchEvent::Closed(
                cluster_sdk::ClusterError::Shutdown,
            ));
            // Then drop the senders. Ordering, not redundancy: the two events above
            // are already queued, and a receiver drains what is queued before it
            // observes the close, so closing first would deliver neither.
            //
            // The `Closed(Shutdown)` is what actually ends an attached stream —
            // `subscription_stream` returns on a terminal event — so this is not
            // what unblocks tonic's graceful shutdown; that was measured by
            // stubbing this call out and watching the regression test still pass.
            // What it covers is the rest of the table: a subscription `join`
            // registered that no `await_change` ever attached to, and a reader
            // whose headroom was exhausted so its close was only best-effort.
            // Before *any* of this existed, nothing here ran at all: no terminal
            // event, no dropped sender, `await_change` never ending server-side,
            // and the 30 s `stop_timeout` severing the stream with `abort()`.
            let closed = subscriptions.close_all();
            if closed > 0 {
                tracing::debug!(
                    closed,
                    "cluster: delivered the shutdown two-step and closed the election subscriptions"
                );
            }
        }

        // Swap the snapshot next, so a request arriving during the drain resolves
        // to `ProfileNotBound` rather than reaching a backend that is about to be
        // torn down (DESIGN §4.8, §5.6 phase C). This mirrors the wiring's own
        // order, where deregistration precedes the plugin stop hooks.
        if let Some(profiles) = self.profiles.get() {
            profiles.clear();
        }

        // Take the handle out before awaiting so the lock isn't held across the
        // shutdown await.
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            tokio::select! {
                () = handle.stop() => {}           // graceful: deregister + compose plugin stops
                () = deadline.cancelled() => {}    // framework deadline elapsed
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "gear_tests.rs"]
mod gear_tests;
