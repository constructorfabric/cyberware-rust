#![allow(clippy::expect_used)]

use std::sync::Arc;

use quota_enforcement_sdk::testing::InMemoryStorage;
use serde_json::json;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use toolkit::config::ConfigProvider;
use toolkit::lifecycle::ReadySignal;
use toolkit::{ClientHub, Gear, GearCtx, HealthcheckResult, RestApiCapability};
use uuid::Uuid;

use super::QuotaEnforcementGear;
use crate::domain::{Dependency, ReadinessState};
use crate::test_support::{
    ClusterFixture, OtherProfile, PermitTenantsPdp, hub_with, register_pdp, register_storage,
    storage_instance, tenant, wire_cluster, wire_cluster_with,
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
                "sweeper_stop_timeout_secs": 1
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

/// Registry with the storage plugin instance, the PDP double, a wired cluster,
/// and optionally the storage double's scoped client.
fn environment(
    with_storage_client: bool,
    cluster: ClusterBinding,
) -> (Arc<ClientHub>, Arc<InMemoryStorage>, ClusterFixture) {
    let storage_fixture = storage_instance("cf.core._.qe_db_storage.v1", "acme", 100);
    let hub = hub_with(&[&storage_fixture]);
    register_pdp(
        &hub,
        Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()])),
    );
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
    assert_eq!(storage.bootstrap_calls(), 1);

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
