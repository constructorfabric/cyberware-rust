#![allow(clippy::expect_used)]

use std::sync::Arc;

use quota_enforcement_sdk::testing::{InMemoryCoordination, InMemoryStorage};
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
    PermitTenantsPdp, coordination_instance, hub_with, register_coordination, register_pdp,
    register_storage, storage_instance, tenant,
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
                "coordination_vendor": "acme",
                "probe_lock_ttl_secs": 1
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

/// Registry with both plugin instances, PDP double, coordination double, and
/// optionally the storage double's scoped client.
fn environment(with_storage_client: bool) -> (Arc<ClientHub>, Arc<InMemoryStorage>) {
    let storage_fixture = storage_instance("cf.core._.qe_db_storage.v1", "acme", 100);
    let coordination_fixture =
        coordination_instance("cf.core._.qe_db_coordination.v1", "acme", 100);
    let hub = hub_with(&[&storage_fixture, &coordination_fixture]);
    register_pdp(
        &hub,
        Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()])),
    );
    register_coordination(
        &hub,
        &coordination_fixture,
        Arc::new(InMemoryCoordination::new()),
    );
    let storage = Arc::new(InMemoryStorage::new());
    if with_storage_client {
        register_storage(&hub, &storage_fixture, storage.clone());
    }
    (hub, storage)
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
    let (hub, storage) = environment(true);
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
    assert!(service.coordination().is_ok());
    assert_eq!(storage.bootstrap_calls(), 1);

    let check = gear.healthcheck(&ctx).expect("health check after init");
    assert_eq!(
        check.check().await.status,
        HealthcheckResult::healthy().status
    );

    cancel.cancel();
    handle
        .await
        .expect("serve task joins")
        .expect("serve returns Ok on shutdown");
}

#[tokio::test]
async fn serve_fails_and_never_signals_ready_when_bootstrap_fails() {
    let (hub, storage) = environment(false);
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
}

#[tokio::test]
async fn serve_stops_cleanly_when_cancelled_during_bootstrap() {
    let (hub, _) = environment(true);
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
}

#[tokio::test]
async fn a_second_init_fails_and_the_health_check_exists_only_after_init() {
    let (hub, _) = environment(true);
    let gear = QuotaEnforcementGear::default();
    let ctx = make_ctx(hub);
    assert!(gear.healthcheck(&ctx).is_none(), "no service, no check");
    gear.init(&ctx).await.expect("first init");
    assert!(gear.healthcheck(&ctx).is_some());
    let err = gear.init(&ctx).await.expect_err("second init");
    assert!(err.to_string().contains("already initialized"), "{err}");
}
