use std::sync::Arc;
use std::time::Duration;

use quota_enforcement_sdk::testing::{InMemoryCoordination, InMemoryStorage};
use quota_enforcement_sdk::{CONTRACT_MAJOR, CoordinationError, LockScope, StorageError};
use toolkit::ClientHub;

use super::Bootstrap;
use crate::domain::error::{Dependency, DomainError};
use crate::domain::plugins::PluginBinding;
use crate::domain::readiness::{Readiness, ReadinessState};
use crate::test_support::{
    PermitTenantsPdp, coordination_instance, hub_with, register_coordination, register_pdp,
    register_storage, storage_instance, tenant,
};

struct Harness {
    hub: Arc<ClientHub>,
    storage: Arc<InMemoryStorage>,
    coordination: Arc<InMemoryCoordination>,
    readiness: Arc<Readiness>,
}

fn harness(storage: Arc<InMemoryStorage>, register_client: bool, with_pdp: bool) -> Harness {
    let storage_fixture = storage_instance("cf.core._.qe_db_storage.v1", "acme", 100);
    let coordination_fixture =
        coordination_instance("cf.core._.qe_db_coordination.v1", "acme", 100);
    let hub = hub_with(&[&storage_fixture, &coordination_fixture]);
    if register_client {
        register_storage(&hub, &storage_fixture, storage.clone());
    }
    let coordination = Arc::new(InMemoryCoordination::new());
    register_coordination(&hub, &coordination_fixture, coordination.clone());
    if with_pdp {
        register_pdp(
            &hub,
            Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()])),
        );
    }
    Harness {
        hub,
        storage,
        coordination,
        readiness: Arc::new(Readiness::new()),
    }
}

fn bootstrap(h: &Harness) -> Bootstrap {
    Bootstrap::new(
        PluginBinding::new(h.hub.clone(), "acme".to_owned(), "acme".to_owned()),
        h.hub.clone(),
        Duration::from_secs(2),
        h.readiness.clone(),
    )
}

#[tokio::test]
async fn a_complete_environment_bootstraps_probes_every_scope_and_becomes_ready() {
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
    assert_eq!(h.coordination.try_lock_calls(), LockScope::ALL.len());
    assert_eq!(h.coordination.release_calls(), LockScope::ALL.len());
    for scope in LockScope::ALL {
        assert!(
            !h.coordination.is_held(scope),
            "probe locks are released: {scope}"
        );
    }
    bound
        .coordination
        .try_lock(LockScope::LeaseSweeper, Duration::from_secs(1))
        .await
        .expect("the bound coordination handle is usable");
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
    assert_eq!(h.coordination.try_lock_calls(), 0, "later steps never run");
}

#[tokio::test]
async fn a_missing_storage_client_fails_bootstrap_before_any_probe() {
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
}

#[tokio::test]
async fn a_failing_coordination_probe_fails_bootstrap_on_the_coordination_dependency() {
    let h = harness(Arc::new(InMemoryStorage::new()), true, true);
    h.coordination
        .fail_with(CoordinationError::BackendUnavailable("down".into()));
    let err = bootstrap(&h).run().await.err().expect("probe fails");
    assert!(
        matches!(
            err,
            DomainError::CoordinationProbeFailed {
                scope: LockScope::LeaseSweeper,
                ..
            }
        ),
        "the first scope is probed first: {err:?}"
    );
    match h.readiness.snapshot() {
        ReadinessState::Failed { dependency, reason } => {
            assert_eq!(dependency, Dependency::Coordination);
            assert!(reason.contains("lease_sweeper"), "{reason}");
        }
        other => panic!("expected a coordination failure, got {other:?}"),
    }
    assert_eq!(
        h.storage.bootstrap_calls(),
        1,
        "storage bootstrap ran before the probe"
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
async fn a_missing_pdp_client_fails_bootstrap_after_the_probes() {
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
    assert_eq!(
        h.coordination.release_calls(),
        LockScope::ALL.len(),
        "probes completed"
    );
}
