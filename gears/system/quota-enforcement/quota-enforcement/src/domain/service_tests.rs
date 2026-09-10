use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use quota_enforcement_sdk::testing::InMemoryStorage;

use super::Service;
use crate::domain::admission::Admission;
use crate::domain::bootstrap::Bound;
use crate::domain::catalog::ProjectionContractCatalog;
use crate::domain::error::{Dependency, DomainError};
use crate::domain::ports::metrics::NoopMetrics;
use crate::domain::quotas::QuotaLimits;
use crate::domain::readiness::Readiness;
use crate::test_support::{
    FakeContractRegistry, FakeMetricRegistry, NoopCoordinator, PermitTenantsPdp, tenant,
};

fn limits() -> QuotaLimits {
    QuotaLimits {
        metadata_max_bytes: 4096,
        list_max_limit: 500,
        list_max_ids: 100,
    }
}

fn service() -> Service {
    let enforcer = PolicyEnforcer::new(Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()])));
    Service::new(
        Admission::new(enforcer, Arc::new(NoopMetrics)),
        Arc::new(Readiness::new()),
        limits(),
    )
}

#[test]
fn dependencies_are_not_ready_until_bound_and_bind_happens_once() {
    let svc = service();
    assert_eq!(
        svc.storage().err(),
        Some(DomainError::NotReady {
            dependency: Dependency::Storage
        })
    );
    assert_eq!(
        svc.coordinator().err(),
        Some(DomainError::NotReady {
            dependency: Dependency::Cluster
        })
    );
    assert_eq!(
        svc.catalog().err(),
        Some(DomainError::NotReady {
            dependency: Dependency::Catalog
        })
    );
    assert!(svc.attribution().is_err(), "no catalogue, no ingress");
    assert_eq!(
        svc.quotas().err(),
        Some(DomainError::NotReady {
            dependency: Dependency::Storage
        }),
        "no storage, no lifecycle"
    );

    let bound = Bound {
        storage: Arc::new(InMemoryStorage::new()),
        coordinator: Arc::new(NoopCoordinator),
        catalog: Arc::new(ProjectionContractCatalog::empty()),
        registry: Arc::new(FakeContractRegistry::empty()),
        metric_registry: Arc::new(FakeMetricRegistry::empty()),
    };
    svc.bind(bound.clone()).expect("first bind");
    assert!(svc.storage().is_ok());
    assert!(svc.coordinator().is_ok());
    assert!(svc.catalog().is_ok());
    assert!(svc.attribution().is_ok());
    assert_eq!(svc.quotas().expect("lifecycle").limits(), limits());
    assert!(matches!(svc.bind(bound), Err(DomainError::Internal(_))));
    assert!(
        !svc.readiness().is_ready(),
        "binding does not imply readiness; bootstrap marks it"
    );
}
