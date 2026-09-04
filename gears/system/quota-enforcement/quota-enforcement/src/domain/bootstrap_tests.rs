use std::sync::Arc;

use quota_enforcement_sdk::testing::InMemoryStorage;
use quota_enforcement_sdk::{CONTRACT_MAJOR, StorageError};
use tokio_util::sync::CancellationToken;
use toolkit::ClientHub;

use super::Bootstrap;
use crate::domain::error::{Dependency, DomainError};
use crate::domain::plugins::PluginBinding;
use crate::domain::ports::coordination::SingletonScope;
use crate::domain::readiness::{Readiness, ReadinessState};
use crate::test_support::{
    PermitTenantsPdp, StaticCoordinatorBinding, hub_with, idle_work, register_pdp,
    register_storage, storage_instance, tenant,
};

struct Harness {
    hub: Arc<ClientHub>,
    storage: Arc<InMemoryStorage>,
    coordinator: Arc<StaticCoordinatorBinding>,
    readiness: Arc<Readiness>,
}

fn harness(storage: Arc<InMemoryStorage>, register_client: bool, with_pdp: bool) -> Harness {
    let storage_fixture = storage_instance("cf.core._.qe_db_storage.v1", "acme", 100);
    let hub = hub_with(&[&storage_fixture]);
    if register_client {
        register_storage(&hub, &storage_fixture, storage.clone());
    }
    if with_pdp {
        register_pdp(
            &hub,
            Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()])),
        );
    }
    Harness {
        hub,
        storage,
        coordinator: StaticCoordinatorBinding::ok(),
        readiness: Arc::new(Readiness::new()),
    }
}

fn bootstrap(h: &Harness) -> Bootstrap {
    Bootstrap::new(
        PluginBinding::new(h.hub.clone(), "acme".to_owned()),
        h.coordinator.clone(),
        h.hub.clone(),
        h.readiness.clone(),
    )
}

#[tokio::test]
async fn a_complete_environment_bootstraps_resolves_the_coordinator_and_becomes_ready() {
    let h = harness(Arc::new(InMemoryStorage::new()), true, true);
    let bound = bootstrap(&h).run().await.expect("bootstrap succeeds");

    assert!(h.readiness.is_ready());
    assert_eq!(h.storage.bootstrap_calls(), 1);
    assert_eq!(
        h.storage.seeded_defaults().map(|d| d.max_active_leases),
        Some(1000),
        "the foundation bundle seeded the PRD defaults"
    );
    assert_eq!(
        h.storage.bootstrapped_bundle().map(|b| b.contract_major),
        Some(CONTRACT_MAJOR)
    );
    assert_eq!(
        h.coordinator.calls(),
        1,
        "the cluster binding resolved once"
    );

    let shutdown = CancellationToken::new();
    shutdown.cancel();
    bound
        .coordinator
        .run_while_leader(SingletonScope::LeaseSweeper, shutdown, idle_work())
        .await
        .expect("the bound coordinator is usable");
}

#[tokio::test]
async fn a_schema_mismatch_fails_bootstrap_on_the_storage_dependency() {
    let h = harness(
        Arc::new(InMemoryStorage::with_installed_schema_major(
            CONTRACT_MAJOR + 1,
        )),
        true,
        true,
    );
    let err = bootstrap(&h).run().await.err().expect("mismatch");
    assert_eq!(
        err,
        DomainError::SchemaVersionMismatch {
            installed: CONTRACT_MAJOR + 1,
            expected: CONTRACT_MAJOR,
        }
    );
    assert!(matches!(
        h.readiness.snapshot(),
        ReadinessState::Failed {
            dependency: Dependency::Storage,
            ..
        }
    ));
    assert_eq!(h.coordinator.calls(), 0, "later steps never run");
}

#[tokio::test]
async fn a_missing_storage_client_fails_bootstrap_before_the_cluster_resolve() {
    let h = harness(Arc::new(InMemoryStorage::new()), false, true);
    let err = bootstrap(&h)
        .run()
        .await
        .err()
        .expect("client not registered");
    assert!(
        matches!(err, DomainError::PluginClientNotRegistered { .. }),
        "{err:?}"
    );
    assert!(matches!(
        h.readiness.snapshot(),
        ReadinessState::Failed {
            dependency: Dependency::Storage,
            ..
        }
    ));
    assert_eq!(h.storage.bootstrap_calls(), 0);
    assert_eq!(h.coordinator.calls(), 0);
}

#[tokio::test]
async fn a_failing_cluster_resolve_fails_bootstrap_on_the_cluster_dependency() {
    let mut h = harness(Arc::new(InMemoryStorage::new()), true, true);
    h.coordinator = StaticCoordinatorBinding::failing(DomainError::ClusterUnavailable(
        "no backend bound for profile `quota-enforcement`".to_owned(),
    ));
    let err = bootstrap(&h).run().await.err().expect("resolve fails");
    assert!(matches!(err, DomainError::ClusterUnavailable(_)), "{err:?}");
    match h.readiness.snapshot() {
        ReadinessState::Failed { dependency, reason } => {
            assert_eq!(dependency, Dependency::Cluster);
            assert!(reason.contains("quota-enforcement"), "{reason}");
        }
        other => panic!("expected a cluster failure, got {other:?}"),
    }
    assert_eq!(
        h.storage.bootstrap_calls(),
        1,
        "storage bootstrap ran before the cluster resolve"
    );
}

#[tokio::test]
async fn a_storage_backend_outage_at_bootstrap_is_unavailability() {
    let storage = Arc::new(InMemoryStorage::new());
    storage.fail_with(StorageError::Unavailable("db down".into()));
    let h = harness(storage, true, true);
    let err = bootstrap(&h).run().await.err().expect("storage down");
    assert_eq!(err, DomainError::BackendUnavailable("db down".to_owned()));
}

#[tokio::test]
async fn a_missing_pdp_client_fails_bootstrap_after_the_cluster_resolve() {
    let h = harness(Arc::new(InMemoryStorage::new()), true, false);
    let err = bootstrap(&h).run().await.err().expect("no PDP");
    assert!(matches!(err, DomainError::PdpUnavailable(_)), "{err:?}");
    assert!(matches!(
        h.readiness.snapshot(),
        ReadinessState::Failed {
            dependency: Dependency::Pdp,
            ..
        }
    ));
    assert_eq!(h.coordinator.calls(), 1, "the cluster resolve completed");
}
