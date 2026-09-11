//! Per-mode service/route gating tests: which routes and services each
//! `DeploymentMode` registers, and which it must not.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationSpec;
use toolkit::config::ConfigProvider;
use toolkit::{ClientHub, Gear, GearCtx, RestApiCapability};

use crate::domain::specification::SpecificationManager;
use tower::ServiceExt;
use uuid::Uuid;

use super::EventBrokerModule;
use crate::config::DeploymentMode;

struct NoopOpenApiRegistry;

impl OpenApiRegistry for NoopOpenApiRegistry {
    fn register_operation(&self, _spec: &OperationSpec) {}

    fn ensure_schema_raw(
        &self,
        name: &str,
        _schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        name.to_owned()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct StaticConfigProvider {
    root: serde_json::Value,
}

impl ConfigProvider for StaticConfigProvider {
    fn get_gear_config(&self, gear: &str) -> Option<&serde_json::Value> {
        self.root.get(gear)
    }
}

/// A fresh temp-file SQLite DB with this crate's migrations applied -
/// `init()` now resolves a real `DBProvider` (`ctx.db_required()`) and runs
/// `SpecificationManager::bulk_load()` against it
/// (eb-single-process-implementation D1), unlike before this change.
async fn test_db_provider() -> toolkit_db::DBProvider<toolkit_db::DbError> {
    let mut path = std::env::temp_dir();
    path.push(format!("cf-eb-module-test-{}.db", Uuid::now_v7().simple()));
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
    toolkit_db::DBProvider::new(db)
}

async fn make_ctx(mode: &str) -> GearCtx {
    make_ctx_with_hub(mode, Arc::new(ClientHub::new())).await
}

/// Registers a default (empty) `MockTypesRegistryClient` on `hub` if the
/// caller hasn't already put one there (`init()` now calls
/// `SpecificationManager::bulk_load()`, which requires one), wires a
/// `standalone` cache provider under the `event-broker` cluster profile if
/// one isn't already bound (`init()` now resolves `EventBrokerCluster` to
/// build `Storage` - eb-single-process-implementation D2 risk mitigation -
/// callers that already wired their own via
/// `test_support::standalone_event_broker_cluster()`, e.g. the
/// service-discovery test below, keep that wiring untouched), and attaches
/// a freshly-migrated test DB (`init()` now calls `ctx.db_required()`).
async fn make_ctx_with_hub(mode: &str, hub: Arc<ClientHub>) -> GearCtx {
    hub.register::<dyn types_registry_sdk::TypesRegistryClient>(Arc::new(
        types_registry_sdk::testing::MockTypesRegistryClient::new(),
    ));
    if crate::domain::cluster::EventBrokerCluster::resolve(&hub)
        .await
        .is_err()
    {
        let config: cluster::ClusterConfig = serde_json::from_value(json!({
            "profiles": { "event-broker": { "cache": { "provider": "standalone" } } }
        }))
        .expect("valid test cluster config");
        let providers = cluster::ProviderRegistry::new()
            .with_cache_provider(Arc::new(standalone_cluster_plugin::StandaloneCacheProvider));
        let handle = cluster::ClusterWiring::from_config(Arc::clone(&hub), &config, &providers)
            .await
            .expect("standalone provider wires cleanly");
        // Intentionally leaked - matches `standalone_event_broker_cluster()`'s
        // own "no graceful shutdown needed in tests" precedent.
        std::mem::forget(handle);
    }
    let cfg = json!({
        "event-broker": {
            "config": {
                "mode": mode,
                "default_storage_backend": "gts.cf.core.events.backend.v1~cf.core.backend.sqlite.v1~",
                // Only load-bearing for cluster_ingest/cluster_delivery
                // (registration `init`-time validation, `eb-dispatcher-routing`
                // design.md D5); harmless for the other two modes.
                "registration": { "advertise_addr": "127.0.0.1:8080" },
            }
        }
    });
    GearCtx::new(
        EventBrokerModule::MODULE_NAME,
        Uuid::new_v4(),
        Arc::new(StaticConfigProvider { root: cfg }),
        hub,
        CancellationToken::new(),
    )
    .with_db(test_db_provider().await)
}

async fn mode_after_init(mode: &str) -> DeploymentMode {
    let module = EventBrokerModule::default();
    let ctx = make_ctx(mode).await;
    module.init(&ctx).await.expect("init must succeed");
    module.mode()
}

#[tokio::test]
async fn standalone_activates_ingest_delivery_and_reaper_but_no_dispatcher() {
    let mode = mode_after_init("standalone").await;
    assert!(mode.ingest_active());
    assert!(mode.delivery_active());
    assert!(!mode.dispatcher_active());
    assert!(mode.reaper_active());
}

#[tokio::test]
async fn cluster_ingest_activates_only_ingest_and_reaper() {
    let mode = mode_after_init("cluster_ingest").await;
    assert!(mode.ingest_active());
    assert!(!mode.delivery_active());
    assert!(!mode.dispatcher_active());
    assert!(mode.reaper_active());
}

#[tokio::test]
async fn cluster_delivery_activates_only_delivery_and_reaper() {
    let mode = mode_after_init("cluster_delivery").await;
    assert!(!mode.ingest_active());
    assert!(mode.delivery_active());
    assert!(!mode.dispatcher_active());
    assert!(mode.reaper_active());
}

#[tokio::test]
async fn cluster_dispatcher_activates_only_the_dispatcher() {
    let mode = mode_after_init("cluster_dispatcher").await;
    assert!(!mode.ingest_active());
    assert!(!mode.delivery_active());
    assert!(mode.dispatcher_active());
    assert!(!mode.reaper_active());
}

#[tokio::test]
async fn init_fails_for_unknown_mode() {
    let module = EventBrokerModule::default();
    let ctx = make_ctx("not_a_real_mode").await;

    let err = module
        .init(&ctx)
        .await
        .expect_err("init must reject an unrecognized mode string rather than silently accepting it or panicking");

    let message = format!("{err:#}");
    assert_eq!(
        message,
        "invalid config for gear 'event-broker': unknown variant `not_a_real_mode`, \
         expected one of `standalone`, `cluster_ingest`, `cluster_delivery`, `cluster_dispatcher`"
    );
}

/// Spec "Advertise-address resolution", "No `advertise_addr`, wildcard bind
/// address, cluster mode" - the `init`-level integration (the resolver-level
/// unit tests live in `infra::cluster::advertise_address::tests`).
#[tokio::test]
async fn init_fails_on_wildcard_bind_with_no_advertise_addr_in_cluster_mode() {
    let cfg = json!({
        "event-broker": {
            "config": {
                "mode": "cluster_ingest",
                "default_storage_backend": "gts.cf.core.events.backend.v1~cf.core.backend.sqlite.v1~",
                // `registration` omitted entirely -> defaults to
                // `listen_addr: "0.0.0.0:0"`, `advertise_addr: None`.
            }
        }
    });
    let ctx = GearCtx::new(
        EventBrokerModule::MODULE_NAME,
        Uuid::new_v4(),
        Arc::new(StaticConfigProvider { root: cfg }),
        Arc::new(ClientHub::new()),
        CancellationToken::new(),
    );

    let module = EventBrokerModule::default();
    let err = module
        .init(&ctx)
        .await
        .expect_err("a wildcard bind with no advertise_addr must fail init in cluster mode");

    assert!(
        format!("{err:#}").contains("wildcard address"),
        "error: {err:#}"
    );
}

/// Spec "Ingest/delivery self-registration": booting in `cluster_ingest`
/// mode registers with `DirectoryService` under `"event-broker-ingest"` at
/// the resolved advertise address.
/// Exactly one role maintains the specification cache. Both halves are
/// asserted against the same seeded registry, because "the dispatcher wrote
/// nothing" only means something next to "the ingest role wrote something".
#[tokio::test]
async fn only_the_ingest_role_maintains_the_specification_cache() {
    const TOPIC_ID: &str = "gts.cf.core.events.topic.v1~x.eb.roles.topic.v1";

    /// `patience` is how long to keep looking. Proving a cache was filled
    /// needs a generous bound under a loaded runtime; proving one was never
    /// filled only needs long enough for a load to have started, since the
    /// initial load is awaited inside `serve()` before anything else is wired.
    async fn topics_after_serve(mode: &str, patience: Duration) -> usize {
        let hub = Arc::new(ClientHub::new());
        let ctx = make_ctx_with_hub(mode, Arc::clone(&hub)).await;
        // After `make_ctx_with_hub`, which registers an empty one of its own.
        hub.register::<dyn types_registry_sdk::TypesRegistryClient>(Arc::new(
            types_registry_sdk::testing::MockTypesRegistryClient::new().with_instances(vec![
                types_registry_sdk::testing::make_test_instance(
                    TOPIC_ID,
                    json!({ "id": TOPIC_ID, "description": "a topic the roles test seeds" }),
                ),
            ]),
        ));
        let db = Arc::new(ctx.db_required().expect("the test ctx carries a database"));

        let module = Arc::new(EventBrokerModule::default());
        module.init(&ctx).await.expect("init must succeed");
        let cancel = CancellationToken::new();
        let serve_module = Arc::clone(&module);
        let serve_cancel = cancel.clone();
        let serve_task = tokio::spawn(async move { serve_module.serve(serve_cancel).await });

        // The initial load is awaited inside `serve()` before anything else is
        // wired, so a cache that is going to be filled is filled by the time
        // the first poll finds the task still running.
        let deadline = Instant::now() + patience;
        let manager =
            crate::infra::specification::TypesRegistrySpecificationManager::new(Arc::clone(&db));
        let count = loop {
            let found = manager.list_topics().await.len();
            if found > 0 || Instant::now() >= deadline {
                break found;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        cancel.cancel();
        let _ = serve_task.await;
        count
    }

    assert_eq!(
        topics_after_serve("cluster_ingest", Duration::from_secs(5)).await,
        1,
        "the ingest role loads and resolves what types-registry holds"
    );
    assert_eq!(
        topics_after_serve("cluster_dispatcher", Duration::from_millis(250)).await,
        0,
        "a dispatcher forwards requests and holds no specification state"
    );
}

#[tokio::test]
async fn cluster_ingest_serve_registers_with_service_discovery() {
    let (hub, _cluster) = crate::test_support::standalone_event_broker_cluster().await;
    let directory = crate::test_support::test_directory_client(&hub);
    let ctx = make_ctx_with_hub("cluster_ingest", Arc::clone(&hub)).await;

    let module = Arc::new(EventBrokerModule::default());
    module.init(&ctx).await.expect("init must succeed");

    let cancel = CancellationToken::new();
    let serve_module = Arc::clone(&module);
    let serve_cancel = cancel.clone();
    let serve_task = tokio::spawn(async move { serve_module.serve(serve_cancel).await });

    // Poll rather than sleep a fixed span. `serve()` runs on another task, and
    // how quickly it reaches `register_instance()` depends on how loaded the
    // machine is - a fixed 20ms yield passed this test alone and failed most
    // full-suite runs, because the rest of the suite competes for the runtime.
    // The deadline is generous because it only bounds a failure; a healthy run
    // leaves this loop on its first or second pass.
    let deadline = Instant::now() + Duration::from_secs(5);
    let instances = loop {
        let found = directory
            .list_instances("event-broker-ingest")
            .await
            .expect("list_instances must not fail");
        if !found.is_empty() || Instant::now() >= deadline {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(instances.len(), 1, "instances: {instances:?}");
    assert_eq!(
        instances[0]
            .rest_endpoint
            .as_ref()
            .map(|ep| ep.uri.as_str()),
        Some("http://127.0.0.1:8080")
    );

    cancel.cancel();
    serve_task
        .await
        .expect("serve task must not panic")
        .expect("serve must return Ok on cancellation");
}

/// De-risks production route registration itself (`eb-rest-handlers` Group
/// 10): `register_ingest_routes`/`register_delivery_routes` register 19
/// `OperationBuilder` operations, three of them (`GET`/`DELETE`/`POST` on
/// `/v1/subscriptions/{id}`) sharing one literal path string across
/// separate `.register()` calls - unlike the `{id}` vs `{seek_id}` case
/// this change's `test_router` had to avoid, axum's `path_router` merges
/// method routers registered against the *same* path string
/// (`axum::routing::path_router::PathRouter::route`), so this is expected
/// to succeed; this test proves it does, not just that it type-checks.
#[tokio::test]
async fn standalone_register_rest_builds_router_and_serves_a_route_with_no_auth_dependency() {
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn authz_resolver_sdk::AuthZResolverApi>(Arc::new(
        crate::test_support::authz_doubles::AllowAllAuthZ,
    ));
    hub.register::<dyn types_registry_sdk::TypesRegistryClient>(Arc::new(
        types_registry_sdk::testing::MockTypesRegistryClient::new(),
    ));
    let ctx = make_ctx_with_hub("standalone", hub).await;
    let module = EventBrokerModule::default();
    module.init(&ctx).await.expect("init must succeed");

    let router = module
        .register_rest(&ctx, Router::new(), &NoopOpenApiRegistry)
        .expect("registering ingest+delivery routes must not fail");

    // `list_topics` takes no `Extension<SecurityContext>` (unlike most
    // other handlers) - real auth middleware is a layer outside this
    // gear's own registration, so this is the one route exercisable
    // end-to-end without a test-only `SecurityContext` shim.
    let request = Request::builder()
        .method("GET")
        .uri("/event-broker/v1/topics")
        .body(Body::empty())
        .expect("request must build");
    let response = router
        .oneshot(request)
        .await
        .expect("the service itself must not error");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body must collect")
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body must be JSON");
    assert_eq!(json["items"], serde_json::json!([]));
}

#[test]
fn for_mode_matches_design_md_deployment_modes_table_exhaustively() {
    // Mirrors `DESIGN.md:2224`'s table row by row - if a fifth mode is ever
    // added, this list forces an explicit decision here too.
    let table = [
        (DeploymentMode::Standalone, (true, true, false, true)),
        (DeploymentMode::ClusterIngest, (true, false, false, true)),
        (DeploymentMode::ClusterDelivery, (false, true, false, true)),
        (
            DeploymentMode::ClusterDispatcher,
            (false, false, true, false),
        ),
    ];

    for (mode, (ingest, delivery, dispatcher, reaper)) in table {
        assert_eq!(mode.ingest_active(), ingest, "ingest mismatch for {mode:?}");
        assert_eq!(
            mode.delivery_active(),
            delivery,
            "delivery mismatch for {mode:?}"
        );
        assert_eq!(
            mode.dispatcher_active(),
            dispatcher,
            "dispatcher mismatch for {mode:?}"
        );
        assert_eq!(mode.reaper_active(), reaper, "reaper mismatch for {mode:?}");
    }

    // Dispatcher is exclusive to cluster_dispatcher; every other mode has
    // no dispatcher constructed (`docs/ADR/0007-service-decomposition.md` D4).
    for (mode, _) in table {
        if mode != DeploymentMode::ClusterDispatcher {
            assert!(
                !mode.dispatcher_active(),
                "{mode:?} must not activate a dispatcher"
            );
        }
    }
}
