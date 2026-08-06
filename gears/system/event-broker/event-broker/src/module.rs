//! `EventBrokerModule`: the `ModKit` gear that wires Ingest/Delivery/
//! Dispatcher/Reaper per deployment mode (`DESIGN.md:2224`'s Deployment
//! Modes table; `docs/ADR/0007-service-decomposition.md`).
//!
//! `init` resolves and stores the configured [`DeploymentMode`], whose
//! `*_active()` predicates report which services/routes *should* exist for
//! that mode. Dispatcher forwarding routes register when
//! `dispatcher_active()` (`eb-dispatcher-routing`); ingest/delivery service
//! construction happens in `register_rest()` against the real `Storage`
//! (eb-single-process-implementation D2 risk mitigation - `InMemoryDomainRepo`
//! is gone). `serve()` self-registers with `DirectoryService` in
//! `cluster_ingest`/`cluster_delivery` mode (design.md D4/D5), starts the
//! ingest outbox pipeline (design.md D5) whenever `ingest_active()`, and
//! otherwise starts no background work.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use axum::Extension;
use event_broker_sdk::EventBrokerBackendProvider;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::client_hub::ClientHub;
use toolkit::directory::{DirectoryClient, RegisterInstanceInfo, ServiceEndpoint};
use toolkit::{DatabaseCapability, Gear, GearCtx, RestApiCapability};
use toolkit_db::outbox::{Outbox, OutboxHandle, Partitions};
use uuid::Uuid;

use crate::api::rest::routes;
use crate::config::{DeploymentMode, EventBrokerConfig, RegistrationConfig, StreamingConfig};
use crate::domain::cluster::EventBrokerCluster;
use crate::domain::outbox::INGEST_OUTBOX_PARTITIONS;
use crate::infra::cluster::{AdvertiseAddressResolver, ConfigAdvertiseAddress};
use crate::infra::dispatcher::DispatcherState;
use crate::infra::storage::Storage;
use crate::infra::workers::IngestOutboxHandler;

/// How often a registered ingest/delivery instance sends a heartbeat to
/// `DirectoryService` to stay routable (design.md D4). A placeholder
/// interval - `docs/DESIGN.md`'s "Open — broker team" note flags that
/// `DirectoryService`'s actual heartbeat-loss timing still needs to be
/// confirmed against the broker's failover budget.
const DIRECTORY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// The storage backend provider assembled from the backend plugins linked into
/// this build; a second backend adds a line here and nowhere else.
///
/// Mirrors the cluster gear's own `provider_registry()`: the plugin implements
/// the SDK's provider trait, the gear names it once at wiring, and nothing else
/// in the gear knows which backend it got. No database handle is passed: a
/// backend opens the storage its own options name, which is what lets a topic's
/// events live somewhere this gear's metadata does not.
fn backend_provider() -> impl EventBrokerBackendProvider {
    sqlite_event_broker_plugin::SqliteBackendProvider
}

/// Expressed as predicates on [`DeploymentMode`] itself so the activation
/// set can never disagree with the mode it's derived from
/// (`docs/ADR/0007-service-decomposition.md` D6).
impl DeploymentMode {
    /// `DESIGN.md:2224`'s Deployment Modes table, column by column.
    #[must_use]
    pub fn ingest_active(self) -> bool {
        matches!(self, Self::Standalone | Self::ClusterIngest)
    }

    #[must_use]
    pub fn delivery_active(self) -> bool {
        matches!(self, Self::Standalone | Self::ClusterDelivery)
    }

    #[must_use]
    pub fn dispatcher_active(self) -> bool {
        matches!(self, Self::ClusterDispatcher)
    }

    /// The `reaper` worker runs in every mode except `cluster_dispatcher`
    /// (a stateless HTTP gateway has nothing for it to reap).
    #[must_use]
    pub fn reaper_active(self) -> bool {
        !matches!(self, Self::ClusterDispatcher)
    }
}

#[toolkit::gear(
    name = "event-broker",
    deps = [cluster, authz_resolver, types_registry],
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s")
)]
#[derive(Default)]
pub struct EventBrokerModule {
    mode: OnceLock<DeploymentMode>,
    client_hub: OnceLock<Arc<ClientHub>>,
    registration: OnceLock<RegistrationConfig>,
    streaming: OnceLock<StreamingConfig>,
    loader: OnceLock<crate::config::LoaderConfig>,
    /// Resolved once in `init()` (`ctx.db_required()`, after migrations have
    /// already run) - shared by `SpecificationManager`'s cache, `Storage`, and
    /// the ingest outbox. Not by the storage backend: the event log lives in a
    /// database that backend opens for itself, so a topic's events can be
    /// somewhere this gear's metadata is not.
    db: OnceLock<Arc<toolkit_db::DBProvider<toolkit_db::DbError>>>,
    /// Built in `init()` (`register_rest()` is sync, so it can no longer
    /// construct this itself) and read by `register_rest()`. The actual
    /// cache-table population (`infra::specification::bulk_load`, a free
    /// function - not a method on this trait, since it never touches the
    /// object itself, only the `TypesRegistryClient`/`db` `init()` also has
    /// in hand) happens later, from `serve()` - see that method's own doc
    /// comment for why.
    spec_manager: OnceLock<Arc<dyn crate::domain::specification::SpecificationManager>>,
    /// The partition caches this instance serves from. Built in `init()` because
    /// two callers need the *same* one: `register_rest()` hands it to the
    /// delivery service so a session can attach readers, and `serve()` hands it
    /// to the loader so those readers get filled.
    topics: OnceLock<Arc<crate::infra::loader::topics::TopicManager>>,
    /// The loader task, spawned by `serve()` because that is what owns the
    /// cancellation token. Joined on shutdown so a fetch is not left in flight
    /// against a closing pool.
    loader_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The whole operator config, kept because the retention worker needs two
    /// parts of it that no other collaborator does: the per-topic settings map
    /// and the deployment's default backend name, which together decide what
    /// bounds each topic is held to.
    config: OnceLock<EventBrokerConfig>,
    /// The retention tick, spawned by `serve()` for the same reason the loader
    /// is - that is what owns the cancellation token. Joined on shutdown so a
    /// pass is not left in flight against a closing pool.
    retention_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The specification refresh loop, in the ingest role only.
    specification_refresh_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The storage backend a topic's events are stored by and read back
    /// from, wrapped in the trivial `SingleBackendResolver` - built in
    /// `init()` alongside `spec_manager` from the backend plugin's provider,
    /// read by `register_rest()`.
    backend_resolver: OnceLock<Arc<dyn crate::domain::backend::BackendResolver>>,
    /// The real `Storage` (`ConsumerGroupRepo`/`CursorRepo`/`SubscriptionRepo`/
    /// `IdempotencyGuard`/`ProducerRegistry`/`ActiveStreamMarker`) -
    /// `InMemoryDomainRepo`'s permanent replacement (eb-single-process-
    /// implementation D2 risk mitigation). Built in `init()`, read by
    /// `register_rest()`; its ingest outbox is wired in later, by `serve()`.
    storage: OnceLock<Arc<Storage>>,
    /// The running ingest outbox pipeline, started by `serve()` when
    /// `mode.ingest_active()` and stopped when `serve()` returns
    /// (design.md D5). `None` in modes with no ingest traffic.
    outbox_handle: Mutex<Option<OutboxHandle>>,
}

#[async_trait]
impl Gear for EventBrokerModule {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: EventBrokerConfig = ctx.config()?;
        // Resolved before anything is wired, so a configuration that names two
        // different backends for one instance fails at startup rather than
        // after half the gear is built.
        let backend_selection = cfg.backend_selection()?;
        self.config
            .set(cfg.clone())
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.mode
            .set(cfg.mode)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.client_hub
            .set(ctx.client_hub())
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        if matches!(
            cfg.mode,
            DeploymentMode::ClusterIngest | DeploymentMode::ClusterDelivery
        ) {
            let bound_addr: SocketAddr =
                cfg.registration.listen_addr.parse().with_context(|| {
                    format!(
                        "invalid registration.listen_addr '{}'",
                        cfg.registration.listen_addr
                    )
                })?;
            // Fail fast at startup rather than only once `serve()` actually
            // registers (design.md D5) - the resolved address itself is
            // discarded; `register_self()` recomputes it (a pure, cheap
            // string operation, not worth caching across the two calls).
            ConfigAdvertiseAddress {
                config: &cfg.registration,
            }
            .resolve(bound_addr)?;
        }
        self.registration
            .set(cfg.registration)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.streaming
            .set(cfg.streaming)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        // Built here, not in `register_rest()` or `serve()`, because both of
        // them need this exact instance - see the field comment.
        self.topics
            .set(crate::infra::wiring::build_topic_manager(&cfg.loader))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;
        self.loader
            .set(cfg.loader)
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        // `DatabaseCapability::migrations()` has already run by this point
        // (`libs/toolkit/src/runtime/host_runtime.rs`'s `run_db_phase()`
        // precedes `init()`), so `event_broker_spec_cache` and friends
        // already exist.
        let db = Arc::new(ctx.db_required()?);
        self.db
            .set(Arc::clone(&db))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        // eb-single-process-implementation D1: the actual cache-table
        // populate (`infra::specification::bulk_load`) happens later, in
        // `serve()` - see that method's own doc comment for why. The
        // manager is usable immediately either way: `get_topic`/etc. query
        // the SQLite cache table directly on every call, so handing
        // services this not-yet-loaded manager now is safe as long as the
        // table is populated before any real request can reach them
        // (guaranteed: REST traffic doesn't arrive until the start phase,
        // `serve()`'s own phase, completes).
        let spec_manager: Arc<dyn crate::domain::specification::SpecificationManager> = Arc::new(
            crate::infra::specification::TypesRegistrySpecificationManager::new(Arc::clone(&db)),
        );
        self.spec_manager
            .set(Arc::clone(&spec_manager))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        // The storage backend comes from a plugin, built through its provider
        // the way the cluster gear builds a cache backend from a cache
        // provider. The gear links the plugin but knows only the
        // `EventBrokerBackend` trait behind it - which is what lets a second
        // backend bring its own storage and its own retention rather than
        // inheriting this one's.
        //
        // Every topic resolves to it regardless of `topic`, so it is built from
        // the selection every configured topic agrees on; binding a topic to the
        // backend its own settings name is the next step, and needs the
        // per-topic settings to be the source of truth first.
        //
        // The database it opens is its own. The one resolved above keeps ingest
        // and delivery metadata - cursors, consumer groups, producers, the
        // specification cache, the ingest outbox - and the event log is not
        // among them.
        let provider = backend_provider();
        // The configured type has to be one this build can serve. Checked here
        // rather than trusted, because a type nothing implements would otherwise
        // reach the one linked backend and be stored by it silently, under a
        // name the operator did not choose.
        if backend_selection.r#type.as_ref() != provider.backend_type() {
            anyhow::bail!(
                "configuration names backend type '{}', which this build does not serve; it links '{}'",
                backend_selection.r#type.as_ref(),
                provider.backend_type(),
            );
        }
        let backend = provider
            .build_backend(&backend_selection.settings)
            .await
            .map_err(|e| anyhow::anyhow!("could not build the storage backend: {e}"))?;
        self.backend_resolver
            .set(Arc::new(
                crate::domain::backend::SingleBackendResolver::new(backend),
            ))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        // eb-single-process-implementation D2 risk mitigation: the real
        // `Storage`, wired against the same `db` and `spec_manager`. Its
        // `ClusterCacheV1` (backing the ephemeral `subscription` namespace)
        // is deliberately NOT resolved here - `ClusterGear` only registers
        // its backends into the `ClientHub` during the platform's *start*
        // phase, which runs after every gear's `init()`
        // (`host_runtime.rs`'s phase order). `serve()` resolves and wires it
        // in once that phase has begun (`Storage::set_cache`'s own doc
        // comment has the full story - found by actually booting the
        // standalone binary, since every test wires the cluster cache
        // directly, bypassing this ordering constraint entirely).
        self.storage
            .set(Arc::new(Storage::new(db, Arc::clone(&spec_manager))))
            .map_err(|_| anyhow::anyhow!("{} module already initialized", Self::MODULE_NAME))?;

        tracing::info!(
            module = Self::MODULE_NAME,
            mode = ?cfg.mode,
            "event-broker deployment-mode wiring resolved"
        );
        Ok(())
    }
}

/// Every table this gear owns (`SpecificationManager`'s cache, `Storage`'s
/// durable namespaces, the ingest outbox) is registered here as one
/// idempotent migration - gears must not receive
/// a raw DB connection (`libs/toolkit/src/contracts.rs`'s `DatabaseCapability`
/// rule). Runs automatically before `init()` on every `Run`
/// (`eb-single-process-implementation` design.md D7's "No versioned
/// migration chain" note) - not a separate `Migrate` step.
impl DatabaseCapability for EventBrokerModule {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        crate::infra::storage::migrations::Migrator::migrations()
    }
}

impl RestApiCapability for EventBrokerModule {
    /// Dispatcher forwarding routes register when `dispatcher_active()`
    /// (`eb-dispatcher-routing`). Ingest/delivery routes register when
    /// `ingest_active()`/`delivery_active()` - both share one `HandlerState`,
    /// built the same way `test_support::harness::EventBrokerHarness` does
    /// (`infra::wiring::build_handler_state`, over the same real `Storage`
    /// both construct their services against).
    ///
    /// Resolves `EventBrokerCluster` once and attaches it (plus a shared
    /// Pingora connector) via `Extension` after registration, matching the
    /// "attach service once after all routes are registered" convention
    /// (`docs/toolkit_unified_system/04_rest_operation_builder.md`).
    fn register_rest(
        &self,
        ctx: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let mode = *self
            .mode
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before register_rest()"))?;

        let mut router = router;
        if mode.ingest_active() {
            router = routes::register_ingest_routes(router, openapi);
        }
        if mode.delivery_active() {
            router = routes::register_delivery_routes(router, openapi);
        }
        if mode.ingest_active() || mode.delivery_active() {
            // The whole struct, not one field: every knob in it is read
            // downstream now - batch bounds and progress cadence by the session,
            // the heartbeat by its schedule.
            let streaming = self
                .streaming
                .get()
                .ok_or_else(|| anyhow::anyhow!("init must run before register_rest()"))?
                .clone();
            // Real clients, not the test harness's permissive-by-default
            // doubles - `infra::wiring::build_handler_state` is this gear's
            // only production `HandlerState` construction path today
            // (`eb-authz-enforcement`'s design.md Context), so this is the
            // call that actually turns enforcement on in production.
            let authz: Arc<dyn AuthZResolverApi> =
                ctx.client_hub().get::<dyn AuthZResolverApi>()?;
            let spec_manager = self
                .spec_manager
                .get()
                .ok_or_else(|| anyhow::anyhow!("init must run before register_rest()"))?
                .clone();
            let backend_resolver = self
                .backend_resolver
                .get()
                .ok_or_else(|| anyhow::anyhow!("init must run before register_rest()"))?
                .clone();
            let storage = self
                .storage
                .get()
                .ok_or_else(|| anyhow::anyhow!("init must run before register_rest()"))?
                .clone();
            // The same `TopicManager` the loader fills, not a second one -
            // see the field comment on `topics`.
            let topics = self
                .topics
                .get()
                .ok_or_else(|| anyhow::anyhow!("init must run before register_rest()"))?
                .clone();
            let state = crate::infra::wiring::build_handler_state(
                storage,
                PolicyEnforcer::new(authz),
                spec_manager,
                backend_resolver,
                topics,
                // Process-local: a group is owned by one delivery instance, so
                // stream exclusion needs no storage and no coordination.
                Arc::new(crate::domain::streaming::lease::InProcessStreamLeases::new()),
                streaming,
            );
            router = router.layer(Extension(state));
        }

        if !mode.dispatcher_active() {
            return Ok(router);
        }
        let router = routes::register_dispatcher_routes(router, openapi);
        let directory: Arc<dyn DirectoryClient> = ctx.client_hub().get::<dyn DirectoryClient>()?;
        let state = Arc::new(DispatcherState::new(directory));
        Ok(router.layer(Extension(state)))
    }
}

impl EventBrokerModule {
    /// Crate-internal introspection point for the per-mode gating tests
    /// (`docs/ADR/0007-service-decomposition.md` D6) - not public API.
    #[cfg(test)]
    pub(crate) fn mode(&self) -> DeploymentMode {
        *self.mode.get().expect("init must run before mode()")
    }

    /// Background lifecycle entry point (`lifecycle(entry = "serve")`).
    /// First does the deferred startup work `init()` couldn't (see the two
    /// calls below), then starts the ingest outbox pipeline (design.md D5)
    /// whenever `mode.ingest_active()`, then self-registers with
    /// `DirectoryService` in `cluster_ingest`/`cluster_delivery` mode
    /// (design.md D4) and heartbeats until cancelled, explicitly
    /// deregistering on shutdown (`DirectoryClient` has no TTL-lease
    /// semantics, unlike the removed `ServiceDiscoveryV1`) - or, in
    /// `standalone`/`cluster_dispatcher` mode, simply awaits cancellation.
    /// The outbox pipeline, if started, is stopped last, after that
    /// mode-specific work returns. The `reaper` worker lands with a future
    /// ticket.
    pub(crate) async fn serve(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let mode = *self
            .mode
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before serve()"))?;

        // Must happen before anything else in this method, and only where the
        // ingest role runs. `types-registry` commits `entities:`-seeded
        // instances from its configuration-mode storage to queryable storage
        // during the platform's `post_init`, which runs after every gear's
        // `init()` but before `serve()`, so a load in `init()` would always see
        // zero of them - discovered by seeding a topic that way and finding
        // `GET /v1/topics` still empty afterwards.
        //
        // Exactly one role writes this state and the others re-read it: a
        // delivery instance reads what ingest resolved out of the shared
        // database, which is why it needs none of the `topics` configuration
        // that shaped it, and a dispatcher holds none of it at all.
        if mode.ingest_active() {
            self.start_specification_refresh(&cancel).await?;
        }

        // Must happen before anything else that follows: `cluster`'s own
        // `start()` (which actually registers its backends into the
        // `ClientHub`) is guaranteed to have already run by the time
        // `serve()` is invoked (topo-sorted dependency order within the
        // platform's start phase - `EventBrokerModule`'s `deps = [cluster,
        // ...]`), but nothing before `serve()` is. See `Storage::set_cache`'s
        // doc comment. Returns the resolved cache so `start_outbox_pipeline`
        // can hand the same handle to `IngestOutboxHandler` (design.md D6's
        // delivery-wake-up notification) without re-resolving it.
        let cluster_cache = self.wire_cluster_cache().await?;

        if mode.ingest_active() {
            self.start_outbox_pipeline(cluster_cache).await?;
        }

        // Only where delivery runs: an ingest-only instance serves no streams,
        // so a loader there would fetch for readers that cannot exist.
        if mode.delivery_active() {
            self.start_loader(&cancel)?;
        }

        // Wherever events are stored: an instance that holds rows is the one
        // that has to keep them bounded, whether or not anything reads them.
        if mode.ingest_active() || mode.delivery_active() {
            self.start_retention(&cancel)?;
        }

        let result = self.serve_directory_presence(mode, &cancel).await;

        let handle = self
            .outbox_handle
            .lock()
            .map_err(|e| anyhow::anyhow!("outbox_handle lock: {e}"))?
            .take();
        if let Some(handle) = handle {
            tracing::info!(
                module = Self::MODULE_NAME,
                "stopping ingest outbox pipeline"
            );
            handle.stop().await;
        }

        self.stop_loader().await;
        self.stop_retention().await;
        self.stop_specification_refresh().await;

        result
    }

    /// The `cluster_ingest`/`cluster_delivery` directory self-registration
    /// and heartbeat loop, or (in `standalone`/`cluster_dispatcher` mode) a
    /// bare wait for cancellation - split out of `serve()` so the outbox
    /// pipeline's start/stop wraps around this uniformly regardless of mode.
    async fn serve_directory_presence(
        &self,
        mode: DeploymentMode,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let gear_name = match mode {
            DeploymentMode::ClusterIngest => Some("event-broker-ingest"),
            DeploymentMode::ClusterDelivery => Some("event-broker-delivery"),
            DeploymentMode::Standalone | DeploymentMode::ClusterDispatcher => None,
        };
        let Some(gear_name) = gear_name else {
            cancel.cancelled().await;
            return Ok(());
        };

        let (directory, info) = self.register_self(gear_name).await?;
        self.presence_loop(&directory, &info, cancel).await;
        if let Err(e) = directory
            .deregister_instance(&info.gear, &info.instance_id)
            .await
        {
            tracing::warn!(
                module = Self::MODULE_NAME,
                gear = gear_name,
                error = %e,
                "deregister on shutdown failed"
            );
        }
        Ok(())
    }

    /// Resolves `EventBrokerCluster` and wires its `cache` into `Storage`
    /// (`Storage::set_cache`'s own doc comment has the full ordering
    /// rationale). Must be called from `serve()` - `cluster`'s backends
    /// aren't registered into the `ClientHub` until the platform's start
    /// phase.
    async fn wire_cluster_cache(&self) -> anyhow::Result<cluster_sdk::ClusterCacheV1> {
        let hub = self
            .client_hub
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before wire_cluster_cache()"))?;
        let storage = self
            .storage
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before wire_cluster_cache()"))?;
        let cache = EventBrokerCluster::resolve(hub)
            .await
            .context("resolving the event-broker cluster profile failed")?
            .cache;
        storage.set_cache(cache.clone());
        Ok(cache)
    }

    /// Builds and starts the ingest outbox pipeline (design.md D5):
    /// `Outbox::builder(db).queue(INGEST_QUEUE_NAME, ..).leased(handler).
    /// start()`, then wires the resulting `Arc<Outbox>` into `Storage` via
    /// [`Storage::set_outbox`] so `IdempotencyGuard::check_and_enqueue` has
    /// somewhere to insert. Stores the handle for `serve()` to stop later.
    /// Joins the loader task.
    ///
    /// It observes the same `cancel` token `serve()` returned on, so it is
    /// already stopping by the time this runs; joining is what makes sure an
    /// in-flight fetch finishes rather than being dropped against a pool that is
    /// about to close.
    async fn stop_loader(&self) {
        let loader = self
            .loader_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(loader) = loader {
            tracing::info!(module = Self::MODULE_NAME, "stopping delivery loader");
            if let Err(err) = loader.await {
                tracing::warn!(?err, "delivery loader did not shut down cleanly");
            }
        }
    }

    /// Joins the specification refresh loop, on the same terms as
    /// [`Self::stop_loader`].
    async fn stop_specification_refresh(&self) {
        let refresh = self
            .specification_refresh_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(refresh) = refresh {
            tracing::info!(module = Self::MODULE_NAME, "stopping specification refresh");
            if let Err(err) = refresh.await {
                tracing::warn!(?err, "specification refresh did not shut down cleanly");
            }
        }
    }

    /// Joins the retention tick, on the same terms as [`Self::stop_loader`].
    async fn stop_retention(&self) {
        let retention = self
            .retention_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(retention) = retention {
            tracing::info!(module = Self::MODULE_NAME, "stopping retention tick");
            if let Err(err) = retention.await {
                tracing::warn!(?err, "retention tick did not shut down cleanly");
            }
        }
    }

    /// Spawns the tick that drives each topic's backend through one retention
    /// pass.
    ///
    /// In `serve()` for the same reasons as the loader: this is what owns the
    /// cancellation token, and spawning needs a runtime.
    /// Loads the specification cache once, awaited, then leaves a loop
    /// refreshing it on the configured cadence.
    ///
    /// The first pass is awaited because nothing may serve a request against an
    /// empty cache, and REST traffic starts arriving as soon as the platform's
    /// start phase completes.
    async fn start_specification_refresh(&self, cancel: &CancellationToken) -> anyhow::Result<()> {
        let db = self
            .db
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before serve()"))?;
        let config = self
            .config
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before serve()"))?;
        let types_registry = self
            .client_hub
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before serve()"))?
            .get::<dyn types_registry_sdk::TypesRegistryClient>()?;

        let worker = crate::infra::workers::SpecificationRefreshWorker::new(
            types_registry,
            Arc::clone(db),
            config.clone(),
            Duration::from_secs(u64::from(
                config.workers.specification_refresh_interval_secs,
            )),
        );
        worker.run_once().await;
        let handle = worker.spawn(cancel.clone());
        let mut guard = self
            .specification_refresh_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(handle);
        Ok(())
    }

    fn start_retention(&self, cancel: &CancellationToken) -> anyhow::Result<()> {
        let config = self
            .config
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before serve()"))?;
        let spec_manager = self
            .spec_manager
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before serve()"))?
            .clone();
        let backend_resolver = self
            .backend_resolver
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before serve()"))?
            .clone();

        let handle = crate::infra::workers::RetentionWorker::new(
            spec_manager,
            backend_resolver,
            config.clone(),
            Duration::from_secs(u64::from(config.workers.retention_interval_secs)),
        )
        .spawn(cancel.clone());
        let mut guard = self
            .retention_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(handle);
        Ok(())
    }

    /// Spawns the loader that fills partition caches ahead of readers.
    ///
    /// In `serve()` rather than `init()` because this is what owns the
    /// cancellation token, and because spawning needs a runtime. Mirrors
    /// `start_outbox_pipeline`, including holding the handle so shutdown can
    /// join it.
    fn start_loader(&self, cancel: &CancellationToken) -> anyhow::Result<()> {
        let topics = self
            .topics
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before serve()"))?;
        let loader_cfg = self
            .loader
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before serve()"))?;
        let spec_manager = self
            .spec_manager
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before serve()"))?
            .clone();
        let backend_resolver = self
            .backend_resolver
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before serve()"))?
            .clone();

        let handle = crate::infra::wiring::spawn_loader(
            loader_cfg,
            Arc::clone(topics),
            spec_manager,
            backend_resolver,
            cancel.clone(),
        );
        let mut guard = self
            .loader_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(handle);
        Ok(())
    }

    async fn start_outbox_pipeline(
        &self,
        cluster_cache: cluster_sdk::ClusterCacheV1,
    ) -> anyhow::Result<()> {
        let db = self
            .db
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before start_outbox_pipeline()"))?;
        let storage = self
            .storage
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before start_outbox_pipeline()"))?;
        let spec_manager = self
            .spec_manager
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before start_outbox_pipeline()"))?
            .clone();
        let backend_resolver = self
            .backend_resolver
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before start_outbox_pipeline()"))?
            .clone();

        let handle = Outbox::builder(db.db())
            .queue(
                crate::domain::outbox::INGEST_QUEUE_NAME,
                Partitions::of(INGEST_OUTBOX_PARTITIONS),
            )
            .leased(IngestOutboxHandler::new(
                spec_manager,
                backend_resolver,
                cluster_cache,
            ))
            .start()
            .await
            .map_err(|e| anyhow::anyhow!("ingest outbox start: {e}"))?;
        storage.set_outbox(Arc::clone(handle.outbox()));

        let mut guard = self
            .outbox_handle
            .lock()
            .map_err(|e| anyhow::anyhow!("outbox_handle lock: {e}"))?;
        *guard = Some(handle);

        tracing::info!(module = Self::MODULE_NAME, "ingest outbox pipeline started");
        Ok(())
    }

    /// Resolves the advertise address (design.md D5) and registers this
    /// instance with `DirectoryService` under `gear_name`
    /// (`"event-broker-ingest"`/`"event-broker-delivery"` - prefixed so this
    /// intra-gear role registration can't collide with another gear's real
    /// name in `DirectoryService`'s flat namespace). Returns the resolved
    /// `DirectoryClient` and the `RegisterInstanceInfo` used, so the caller
    /// can heartbeat/deregister with the same identity.
    async fn register_self(
        &self,
        gear_name: &str,
    ) -> anyhow::Result<(Arc<dyn DirectoryClient>, RegisterInstanceInfo)> {
        let hub = self
            .client_hub
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before register_self()"))?;
        let registration = self
            .registration
            .get()
            .ok_or_else(|| anyhow::anyhow!("init must run before register_self()"))?;

        let bound_addr: SocketAddr = registration.listen_addr.parse().with_context(|| {
            format!(
                "invalid registration.listen_addr '{}'",
                registration.listen_addr
            )
        })?;
        let address = ConfigAdvertiseAddress {
            config: registration,
        }
        .resolve(bound_addr)?;

        let directory: Arc<dyn DirectoryClient> = hub.get::<dyn DirectoryClient>()?;
        let info = RegisterInstanceInfo::new(gear_name.to_owned(), Uuid::new_v4().to_string())
            .with_rest_endpoint(ServiceEndpoint::new(address));
        directory.register_instance(info.clone()).await?;
        tracing::info!(
            module = Self::MODULE_NAME,
            gear = gear_name,
            instance_id = %info.instance_id,
            "registered with DirectoryService"
        );
        Ok((directory, info))
    }

    /// Sends a heartbeat every [`DIRECTORY_HEARTBEAT_INTERVAL`] until
    /// `cancel` fires. Unlike `toolkit::runtime`'s own (private)
    /// `presence_loop`, this doesn't self-heal by re-registering on
    /// heartbeat failure - a simplification acceptable for this gear's
    /// intra-role registration, revisit if `DirectoryService` restarts turn
    /// out to be disruptive in practice.
    async fn presence_loop(
        &self,
        directory: &Arc<dyn DirectoryClient>,
        info: &RegisterInstanceInfo,
        cancel: &CancellationToken,
    ) {
        let mut heartbeat = tokio::time::interval(DIRECTORY_HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                _ = heartbeat.tick() => {
                    if let Err(e) = directory.send_heartbeat(&info.gear, &info.instance_id).await {
                        tracing::warn!(
                            module = Self::MODULE_NAME,
                            gear = %info.gear,
                            error = %e,
                            "heartbeat failed"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "module_tests.rs"]
mod module_tests;
