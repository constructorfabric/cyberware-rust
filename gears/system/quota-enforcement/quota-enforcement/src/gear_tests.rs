#![allow(clippy::expect_used)]

use std::sync::Arc;

use authz_resolver_sdk::AuthZResolverApi;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Extension, Router};
use quota_enforcement_sdk::testing::InMemoryStorage;
use quota_enforcement_sdk::{PageRequest, QuotaFilter, QuotaManagerClientV1};
use serde_json::json;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use toolkit::api::openapi_registry::OpenApiRegistryImpl;
use toolkit::config::ConfigProvider;
use toolkit::lifecycle::ReadySignal;
use toolkit::{ClientHub, Gear, GearCtx, HealthcheckResult, RestApiCapability};
use toolkit_canonical_errors::CanonicalError;
use tower::ServiceExt as _;
use uuid::Uuid;

use super::QuotaEnforcementGear;
use crate::api::rest::routes::PATH_PREFIX;
use crate::domain::{Dependency, ReadinessState};
use crate::test_support::{
    ClusterFixture, FailingPdp, LLM_MODEL_RESOURCE, LLM_TENANT_PROJECTION, LLM_USER_PROJECTION,
    OtherProfile, PermitTenantsPdp, ctx as security_ctx, hub_with_registry, in_process_registry,
    llm_gateway_documents, metric_base_documents, plugin_instance_document, register_pdp,
    register_storage, storage_instance, tenant, wire_cluster, wire_cluster_with,
};

struct StaticConfigProvider {
    root: serde_json::Value,
}

impl ConfigProvider for StaticConfigProvider {
    fn get_gear_config(&self, gear: &str) -> Option<&serde_json::Value> {
        self.root.get(gear)
    }
}

fn make_ctx(hub: Arc<ClientHub>) -> GearCtx {
    let cfg = json!({
        "quota-enforcement": {
            "config": {
                "storage_vendor": "acme",
                "election": { "ttl_secs": 1, "max_missed_renewals": 1 },
                "sweeper_stop_timeout_secs": 1,
                "quotas": { "metadata_max_bytes": 1024, "list_max_limit": 50 },
                "gauges": { "refresh_secs": 1, "refresh_deadline_secs": 1, "stale_after_secs": 3 },
                "catalog": {
                    "subject_projections": [LLM_USER_PROJECTION, LLM_TENANT_PROJECTION],
                    "resource_projections": [LLM_MODEL_RESOURCE]
                }
            }
        }
    });
    GearCtx::new(
        QuotaEnforcementGear::MODULE_NAME,
        Uuid::from_u128(1),
        Arc::new(StaticConfigProvider { root: cfg }),
        hub,
        CancellationToken::new(),
    )
}

/// Which cluster profile the test binds.
#[derive(Clone, Copy)]
enum ClusterBinding {
    /// The `quota-enforcement` profile over the standalone backend.
    QuotaEnforcement,
    /// A profile the gear never resolves, so its own profile is unbound.
    Other,
}

/// Registry with the storage plugin instance, a permitting PDP double, a wired
/// cluster, and optionally the storage double's scoped client.
fn environment(
    with_storage_client: bool,
    cluster: ClusterBinding,
) -> (Arc<ClientHub>, Arc<InMemoryStorage>, ClusterFixture) {
    environment_with_pdp(
        with_storage_client,
        cluster,
        Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()])),
    )
}

/// [`environment`] with the given PDP double registered as the client. The
/// types registry is the real in-process one, holding the storage plugin
/// instance, the `llm_gateway` owner set, and the two metrics it admits.
fn environment_with_pdp(
    with_storage_client: bool,
    cluster: ClusterBinding,
    pdp: Arc<dyn AuthZResolverApi>,
) -> (Arc<ClientHub>, Arc<InMemoryStorage>, ClusterFixture) {
    let storage_fixture = storage_instance("cf.core._.qe_db_storage.v1", "acme", 100);
    let mut documents = llm_gateway_documents();
    documents.extend(metric_base_documents());
    documents.push(plugin_instance_document(&storage_fixture));
    let hub = hub_with_registry(in_process_registry(documents));
    register_pdp(&hub, pdp);
    let storage = Arc::new(InMemoryStorage::new());
    if with_storage_client {
        register_storage(&hub, &storage_fixture, storage.clone());
    }
    let fixture = match cluster {
        ClusterBinding::QuotaEnforcement => wire_cluster(&hub),
        ClusterBinding::Other => wire_cluster_with(&hub, OtherProfile, None),
    };
    (hub, storage, fixture)
}

#[tokio::test]
async fn init_fails_closed_without_an_authz_resolver_client() {
    let hub = Arc::new(ClientHub::new());
    let gear = QuotaEnforcementGear::default();
    let err = gear
        .init(&make_ctx(hub))
        .await
        .expect_err("no PDP, no gear");
    assert!(format!("{err:#}").contains("authz-resolver"), "{err:#}");
    assert!(gear.service().is_none());
}

#[tokio::test]
async fn init_fails_closed_without_a_types_registry_client() {
    let hub = Arc::new(ClientHub::new());
    register_pdp(
        &hub,
        Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()])),
    );
    let gear = QuotaEnforcementGear::default();
    let err = gear
        .init(&make_ctx(hub))
        .await
        .expect_err("no registry, no catalogue, no gear");
    assert!(format!("{err:#}").contains("types-registry"), "{err:#}");
    assert!(gear.service().is_none());
}

#[tokio::test]
async fn init_then_serve_bootstraps_signals_ready_and_stops_on_cancel() {
    let (hub, storage, fixture) = environment(true, ClusterBinding::QuotaEnforcement);
    let gear = Arc::new(QuotaEnforcementGear::default());
    let ctx = make_ctx(hub);
    gear.init(&ctx).await.expect("init");
    let service = gear.service().expect("service published by init");
    assert!(
        !service.readiness().is_ready(),
        "init never bootstraps; the lifecycle entry does"
    );
    assert!(
        service.storage().is_err(),
        "dependencies are bound by serve"
    );

    let (tx, rx) = oneshot::channel();
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(
        gear.clone()
            .serve(cancel.clone(), ReadySignal::from_sender(tx)),
    );

    rx.await.expect("the ready signal fires after bootstrap");
    assert!(service.readiness().is_ready());
    assert!(service.storage().is_ok());
    assert!(
        service.coordinator().is_ok(),
        "the cluster election was resolved in start"
    );
    let catalog = service
        .catalog()
        .expect("the catalogue was published by bootstrap");
    assert!(
        catalog
            .subject_projection(&gts::GtsTypeId::new(LLM_USER_PROJECTION))
            .is_some(),
        "the configured projection resolved from the real registry"
    );
    assert!(service.attribution().is_ok());
    assert!(
        service.quotas().is_ok(),
        "the quota lifecycle is served once storage is bound"
    );
    assert_eq!(storage.bootstrap_calls(), 1);

    // The elected replica publishes the storage-backed gauge sample within one
    // refresh interval; before the sample lands the gauges observe nothing.
    let cell = gear.lifecycle_gauges().expect("gauge cell after init");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while cell.load().is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the leader never published a gauge sample"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let sample = cell.load().expect("sample");
    assert_eq!(sample.cap_zero, 0);
    assert_eq!(sample.cap_unbounded, 0);

    // One REST round trip through the gear's own route registration.
    let router = gear
        .register_rest(&ctx, Router::new(), &OpenApiRegistryImpl::new())
        .expect("routes registered after init")
        .layer(Extension(security_ctx()));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("{PATH_PREFIX}/quotas"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    // The in-process manager client registered by init answers from the hub.
    let client = ctx
        .client_hub()
        .get::<dyn QuotaManagerClientV1>()
        .expect("manager client published by init");
    let page = client
        .read_quotas(
            &security_ctx(),
            QuotaFilter::default(),
            PageRequest::first(10),
        )
        .await
        .expect("read through the in-process client");
    assert!(page.items.is_empty());

    let check = gear.healthcheck(&ctx).expect("health check after init");
    let result = check.check().await;
    assert_eq!(
        result.status,
        HealthcheckResult::healthy().status,
        "bootstrap is ready and the cluster requirements are met: {result:?}"
    );

    cancel.cancel();
    handle
        .await
        .expect("serve task joins")
        .expect("serve returns Ok on shutdown");
    assert!(cell.load().is_none(), "shutdown withdraws the gauge sample");
    fixture.stop().await;
}

#[tokio::test]
async fn the_in_process_client_answers_not_ready_before_bootstrap_binds_storage() {
    let (hub, _storage, fixture) = environment(true, ClusterBinding::QuotaEnforcement);
    let gear = QuotaEnforcementGear::default();
    let ctx = make_ctx(hub);
    gear.init(&ctx).await.expect("init");
    let client = ctx
        .client_hub()
        .get::<dyn QuotaManagerClientV1>()
        .expect("manager client published by init");
    let err = client
        .read_quotas(
            &security_ctx(),
            QuotaFilter::default(),
            PageRequest::default(),
        )
        .await
        .expect_err("storage is bound by serve, not init");
    assert!(
        matches!(err, CanonicalError::ServiceUnavailable { .. }),
        "{err:?}"
    );
    assert!(
        gear.lifecycle_gauges()
            .expect("gauge cell after init")
            .load()
            .is_none(),
        "no leader, no sample"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn serve_fails_and_never_signals_ready_when_bootstrap_fails() {
    let (hub, storage, fixture) = environment(false, ClusterBinding::QuotaEnforcement);
    let gear = Arc::new(QuotaEnforcementGear::default());
    let ctx = make_ctx(hub);
    gear.init(&ctx).await.expect("init");

    let (tx, rx) = oneshot::channel();
    let cancel = CancellationToken::new();
    let err = gear
        .clone()
        .serve(cancel, ReadySignal::from_sender(tx))
        .await
        .expect_err("bootstrap fails without a storage client");
    assert!(format!("{err:#}").contains("bootstrap"), "{err:#}");
    assert!(rx.await.is_err(), "the ready signal is never sent");
    assert_eq!(storage.bootstrap_calls(), 0);

    let service = gear.service().expect("service");
    assert!(matches!(
        service.readiness().snapshot(),
        ReadinessState::Failed {
            dependency: Dependency::Storage,
            ..
        }
    ));
    let check = gear.healthcheck(&ctx).expect("health check");
    let result = check.check().await;
    assert_eq!(result.status, HealthcheckResult::unhealthy("x").status);
    assert_eq!(result.code.as_deref(), Some("qe_storage_unavailable"));
    fixture.stop().await;
}

#[tokio::test]
async fn serve_fails_on_the_pdp_dependency_when_the_registered_pdp_does_not_answer() {
    // Init accepts the registered client; the bootstrap probe finds the PDP
    // behind it unreachable, so the gear never reports ready.
    let (hub, storage, fixture) =
        environment_with_pdp(true, ClusterBinding::QuotaEnforcement, Arc::new(FailingPdp));
    let gear = Arc::new(QuotaEnforcementGear::default());
    let ctx = make_ctx(hub);
    gear.init(&ctx)
        .await
        .expect("init sees a registered client");

    let (tx, rx) = oneshot::channel();
    let err = gear
        .clone()
        .serve(CancellationToken::new(), ReadySignal::from_sender(tx))
        .await
        .expect_err("the PDP does not answer");
    assert!(
        format!("{err:#}").contains("authorization service unavailable"),
        "{err:#}"
    );
    assert!(rx.await.is_err(), "the ready signal is never sent");
    assert_eq!(
        storage.bootstrap_calls(),
        1,
        "storage bootstrap and the cluster resolve ran before the PDP probe"
    );

    let service = gear.service().expect("service");
    assert!(matches!(
        service.readiness().snapshot(),
        ReadinessState::Failed {
            dependency: Dependency::Pdp,
            ..
        }
    ));
    let check = gear.healthcheck(&ctx).expect("health check");
    let result = check.check().await;
    assert_eq!(result.code.as_deref(), Some("qe_pdp_unavailable"));
    fixture.stop().await;
}

#[tokio::test]
async fn serve_fails_on_the_cluster_dependency_when_the_profile_is_unbound() {
    let (hub, storage, fixture) = environment(true, ClusterBinding::Other);
    let gear = Arc::new(QuotaEnforcementGear::default());
    let ctx = make_ctx(hub);
    gear.init(&ctx).await.expect("init");

    let (tx, rx) = oneshot::channel();
    let err = gear
        .clone()
        .serve(CancellationToken::new(), ReadySignal::from_sender(tx))
        .await
        .expect_err("the quota-enforcement profile is not bound");
    assert!(
        format!("{err:#}").contains("cluster unavailable"),
        "{err:#}"
    );
    assert!(rx.await.is_err(), "the ready signal is never sent");
    assert_eq!(
        storage.bootstrap_calls(),
        1,
        "storage bootstrap ran before the cluster resolve"
    );

    let service = gear.service().expect("service");
    match service.readiness().snapshot() {
        ReadinessState::Failed { dependency, reason } => {
            assert_eq!(dependency, Dependency::Cluster);
            assert!(reason.contains("quota-enforcement"), "{reason}");
        }
        other => panic!("expected a cluster failure, got {other:?}"),
    }
    let check = gear.healthcheck(&ctx).expect("health check");
    let result = check.check().await;
    assert_eq!(result.code.as_deref(), Some("qe_cluster_unavailable"));
    fixture.stop().await;
}

#[tokio::test]
async fn serve_stops_cleanly_when_cancelled_during_bootstrap() {
    let (hub, _, fixture) = environment(true, ClusterBinding::QuotaEnforcement);
    let gear = Arc::new(QuotaEnforcementGear::default());
    gear.init(&make_ctx(hub)).await.expect("init");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (tx, rx) = oneshot::channel();
    let err = gear
        .serve(cancel, ReadySignal::from_sender(tx))
        .await
        .expect_err("cancelled before bootstrap");
    assert!(format!("{err:#}").contains("shutdown"), "{err:#}");
    assert!(rx.await.is_err());
    fixture.stop().await;
}

#[tokio::test]
async fn a_second_init_fails_and_the_health_check_exists_only_after_init() {
    let (hub, _, fixture) = environment(true, ClusterBinding::QuotaEnforcement);
    let gear = QuotaEnforcementGear::default();
    let ctx = make_ctx(hub);
    assert!(gear.healthcheck(&ctx).is_none(), "no service, no check");
    gear.init(&ctx).await.expect("first init");
    assert!(gear.healthcheck(&ctx).is_some());
    let err = gear.init(&ctx).await.expect_err("second init");
    assert!(err.to_string().contains("already initialized"), "{err}");
    fixture.stop().await;
}
