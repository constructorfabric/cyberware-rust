//! Top-level test harness that wires all components together.

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use toolkit_db::outbox::{Outbox, Partitions};
use toolkit_security::SecurityContext;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use uuid::Uuid;

use crate::api::rest::routes::test_router;
use crate::api::rest::state::HandlerState;
use crate::domain::backend::{BackendResolver, SingleBackendResolver};
use crate::domain::delivery::DeliveryService;
use crate::domain::ingest::IngestService;
use crate::domain::outbox::INGEST_QUEUE_NAME;
use crate::domain::specification::SpecificationManager;
use crate::infra::specification::TypesRegistrySpecificationManager;
use crate::infra::storage::Storage;
use crate::infra::workers::IngestOutboxHandler;
use sqlite_event_broker_plugin::{EventLogPath, SqliteEventBackend};

/// A fresh temp-file `SQLite` DB (`SQLite` has no row-level locking, so this
/// stays single-process, matching `eb-single-process-implementation` D3),
/// migrated with every table this crate owns (`SpecificationManager`'s
/// cache, `Storage`'s durable namespaces, the `SQLite` `EventBrokerBackend`,
/// the ingest outbox) via the same `Migrator`
/// `EventBrokerModule::migrations()` runs in production. One shared
/// connection pool (`max_conns: 1`) for every table family - `Storage` and
/// the backend are two different types over the *same* underlying `SQLite`
/// file, matching production (`module.rs::init()` resolves one `db` and
/// hands it to both).
async fn test_db() -> Arc<toolkit_db::DBProvider<toolkit_db::DbError>> {
    let mut path = std::env::temp_dir();
    path.push(format!("cf-eb-harness-{}.db", Uuid::now_v7().simple()));
    let mut file = path.to_string_lossy().replace('\\', "/");
    if !file.starts_with('/') {
        file.insert(0, '/');
    }
    let dsn = format!("sqlite://{file}?mode=rwc");
    let opts = toolkit_db::ConnectOpts {
        max_conns: Some(1),
        min_conns: Some(1),
        ..Default::default()
    };
    let db = toolkit_db::connect_db(&dsn, opts)
        .await
        .expect("connect sqlite");
    toolkit_db::migration_runner::run_migrations_for_testing(
        &db,
        <crate::infra::storage::migrations::Migrator as sea_orm_migration::MigratorTrait>::migrations(),
    )
    .await
    .expect("migrations");
    Arc::new(toolkit_db::DBProvider::new(db))
}

/// Seeds a `TypesRegistrySpecificationManager` from `registry`'s
/// `Topic`/`EventType` fixtures via a `MockTypesRegistryClient`, run through
/// the same startup bulk-load path production uses (`module.rs::serve()`) -
/// not a direct in-memory insert, so harness-backed tests exercise the real
/// `types-registry`-to-cache pipeline (eb-single-process-implementation D2
/// risk mitigation).
async fn seeded_spec_manager(
    db: Arc<toolkit_db::DBProvider<toolkit_db::DbError>>,
    registry: Option<super::type_registry::StaticTypesRegistry>,
) -> Arc<dyn SpecificationManager> {
    let mut instances = Vec::new();
    let mut schemas = Vec::new();
    let mut config = StaticTypesRegistry::empty_config();
    if let Some(registry) = registry {
        for document in registry.topics {
            let id = document["id"]
                .as_str()
                .expect("a topic document names its id");
            instances.push(make_test_instance(id, document.clone()));
        }
        schemas = registry.event_types;
        config = registry.config;
    }
    let client: Arc<dyn types_registry_sdk::TypesRegistryClient> = Arc::new(
        MockTypesRegistryClient::new()
            .with_instances(instances)
            .with_type_schemas(schemas),
    );
    crate::infra::specification::bulk_load(&client, &db, &config)
        .await
        .expect("SpecificationManager bulk-load must not fail");
    Arc::new(TypesRegistrySpecificationManager::new(db))
}

/// The real, permanent `SQLite` `EventBrokerBackend` (eb-single-process-
/// implementation D3) wrapped in a `SingleBackendResolver`, plus the ingest
/// outbox pipeline draining into it - the harness's `Storage` is wired to
/// the same outbox handle via `Storage::set_outbox`, matching
/// `EventBrokerModule::serve()`'s production sequencing. The returned
/// `OutboxHandle` MUST be kept alive for the harness's lifetime - unlike a
/// `ClusterHandle`, dropping an `OutboxHandle` without calling `.stop()`
/// does not leave it harmlessly running: its own doc comment states
/// `TaskSet::Drop` cancels the pipeline's cancellation token on drop,
/// stopping every background worker immediately (discovered the hard way -
/// a dropped-at-function-return handle here meant nothing ever drained).
async fn start_outbox(
    db: Arc<toolkit_db::DBProvider<toolkit_db::DbError>>,
    spec_manager: Arc<dyn SpecificationManager>,
    storage: &Storage,
    cluster_cache: cluster_sdk::ClusterCacheV1,
) -> (
    Arc<dyn event_broker_sdk::EventBrokerBackend>,
    Arc<dyn BackendResolver>,
    toolkit_db::outbox::OutboxHandle,
) {
    // The backend's own event log, not the harness's `db`: that database keeps
    // the metadata behind `Storage` and the ingest outbox, and events are not
    // metadata. In memory, because a harness outlives no process.
    let backend = Arc::new(
        SqliteEventBackend::open(&EventLogPath::InMemory)
            .await
            .expect("an in-memory event log must open"),
    );
    let backend_resolver: Arc<dyn BackendResolver> = Arc::new(SingleBackendResolver::new(
        Arc::clone(&backend) as Arc<dyn event_broker_sdk::EventBrokerBackend>,
    ));

    let handle = Outbox::builder(db.db())
        .queue(INGEST_QUEUE_NAME, Partitions::of(4))
        .leased(IngestOutboxHandler::new(
            Arc::clone(&spec_manager),
            Arc::clone(&backend_resolver),
            cluster_cache,
        ))
        .start()
        .await
        .expect("outbox start");
    storage.set_outbox(Arc::clone(handle.outbox()));

    (
        Arc::clone(&backend) as Arc<dyn event_broker_sdk::EventBrokerBackend>,
        backend_resolver,
        handle,
    )
}

use super::api_v1::ApiV1;
use super::authz_doubles::AllowAllAuthZ;
use super::type_registry::StaticTypesRegistry;

/// Fully-wired test environment for `event-broker` integration tests.
pub struct EventBrokerHarness {
    ingest: Arc<dyn IngestService>,
    delivery: Arc<dyn DeliveryService>,
    storage: Arc<Storage>,
    ctx: SecurityContext,
    router: axum::Router,
    /// Kept alive for the harness's lifetime, never `.stop()`'d - see
    /// `start_outbox`'s doc comment for why dropping it early breaks
    /// draining. Never read after construction, hence `_`-prefixed.
    _outbox_handle: toolkit_db::outbox::OutboxHandle,
    /// The caches the loader fills and a session reads. Exposed so a test can
    /// assert on residency without reaching through the delivery service.
    topics: Arc<crate::infra::loader::topics::TopicManager>,
    /// Stream exclusion, so a test can assert a denied open left no lease held.
    leases: Arc<crate::domain::streaming::lease::InProcessStreamLeases>,
    /// The storage the events actually land in. Exposed because retention is
    /// the backend's own business: a test that needs a prefix-erased or an
    /// emptied partition has to ask the backend to run a pass.
    backend: Arc<dyn event_broker_sdk::EventBrokerBackend>,
    /// Cancels the loader on drop, so a test's loader does not outlive it and
    /// keep fetching against a closing pool.
    loader_shutdown: tokio_util::sync::CancellationToken,
    _loader_handle: tokio::task::JoinHandle<()>,
}

impl Drop for EventBrokerHarness {
    fn drop(&mut self) {
        self.loader_shutdown.cancel();
    }
}

impl EventBrokerHarness {
    /// The partition caches the loader fills.
    #[must_use]
    pub fn topics(&self) -> &Arc<crate::infra::loader::topics::TopicManager> {
        &self.topics
    }

    /// Stream exclusion for this instance.
    #[must_use]
    pub fn leases(&self) -> &Arc<crate::domain::streaming::lease::InProcessStreamLeases> {
        &self.leases
    }

    /// The backend behind every topic here, for a test that has to run a
    /// retention pass or read what storage holds.
    #[must_use]
    pub fn backend(&self) -> &Arc<dyn event_broker_sdk::EventBrokerBackend> {
        &self.backend
    }

    pub fn builder() -> EventBrokerHarnessBuilder {
        EventBrokerHarnessBuilder::default()
    }

    pub fn api_v1(&self) -> ApiV1<'_> {
        ApiV1::new(self)
    }

    pub fn ingest(&self) -> &dyn IngestService {
        &*self.ingest
    }

    pub fn delivery(&self) -> &dyn DeliveryService {
        &*self.delivery
    }

    /// Low-level access to the backing `Storage` - topic/event-type
    /// fixtures go through `EventBrokerHarnessBuilder::with_type_registry`
    /// instead; this stays for whatever else a test needs the real repo
    /// for (`ConsumerGroupRepo`/`CursorRepo`/`SubscriptionRepo`/
    /// `ActiveStreamMarker` - `Storage` implements all of them).
    pub fn repo(&self) -> &Storage {
        &self.storage
    }

    pub fn security_context(&self) -> &SecurityContext {
        &self.ctx
    }

    pub fn router(&self) -> &axum::Router {
        &self.router
    }
}

/// Builder for [`EventBrokerHarness`].
#[derive(Default)]
pub struct EventBrokerHarnessBuilder {
    type_registry: Option<StaticTypesRegistry>,
    policy_enforcer: Option<PolicyEnforcer>,
}

impl EventBrokerHarnessBuilder {
    /// Seeds `Topic`/`EventType` fixtures from a [`StaticTypesRegistry`].
    #[must_use]
    pub fn with_type_registry(mut self, registry: StaticTypesRegistry) -> Self {
        self.type_registry = Some(registry);
        self
    }

    /// Overrides the default always-allow `PolicyEnforcer` (default:
    /// `AllowAllAuthZ`) - for tests asserting a specific authz denial.
    #[must_use]
    pub fn with_policy_enforcer(mut self, policy_enforcer: PolicyEnforcer) -> Self {
        self.policy_enforcer = Some(policy_enforcer);
        self
    }

    #[must_use]
    pub async fn build(self) -> EventBrokerHarness {
        let policy_enforcer = self
            .policy_enforcer
            .unwrap_or_else(|| PolicyEnforcer::new(Arc::new(AllowAllAuthZ)));

        let db = test_db().await;
        let spec_manager = seeded_spec_manager(Arc::clone(&db), self.type_registry).await;

        let (_hub, cluster) = crate::test_support::standalone_event_broker_cluster().await;
        let storage = Arc::new(Storage::new(Arc::clone(&db), Arc::clone(&spec_manager)));
        storage.set_cache(cluster.cache.clone());

        let (backend, backend_resolver, outbox_handle) = start_outbox(
            Arc::clone(&db),
            Arc::clone(&spec_manager),
            &storage,
            cluster.cache,
        )
        .await;

        // The harness runs a real loader over a real `TopicManager`, so a test
        // that opens a stream exercises the production fill path rather than a
        // hand-fed cache. The tick is short because a test should not wait out a
        // production cadence.
        let loader_cfg = crate::config::LoaderConfig {
            tick_ms: 2,
            ..crate::config::LoaderConfig::default()
        };
        let topics = crate::infra::wiring::build_topic_manager(&loader_cfg);
        let leases = Arc::new(crate::domain::streaming::lease::InProcessStreamLeases::new());
        let loader_shutdown = tokio_util::sync::CancellationToken::new();
        let loader_handle = crate::infra::wiring::spawn_loader(
            &loader_cfg,
            Arc::clone(&topics),
            Arc::clone(&spec_manager),
            Arc::clone(&backend_resolver),
            loader_shutdown.clone(),
        );

        let HandlerState { ingest, delivery } = crate::infra::wiring::build_handler_state(
            Arc::clone(&storage),
            policy_enforcer,
            spec_manager,
            backend_resolver,
            Arc::clone(&topics),
            Arc::clone(&leases),
            // Short cadences so idle behaviour is testable without real waits.
            // Everything else is the production default.
            crate::config::StreamingConfig {
                heartbeat_interval_secs: 1,
                ..crate::config::StreamingConfig::default()
            },
        );

        let ctx = SecurityContext::builder()
            .subject_tenant_id(Uuid::new_v4())
            .subject_id(Uuid::new_v4())
            .build()
            .expect("test security context");

        let router = test_router(
            HandlerState {
                ingest: Arc::clone(&ingest),
                delivery: Arc::clone(&delivery),
            },
            ctx.clone(),
        );

        EventBrokerHarness {
            ingest,
            delivery,
            storage,

            ctx,
            router,
            _outbox_handle: outbox_handle,
            topics,
            leases,
            backend,
            loader_shutdown,
            _loader_handle: loader_handle,
        }
    }
}
