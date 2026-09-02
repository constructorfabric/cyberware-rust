use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use quota_enforcement_sdk::testing::{InMemoryCoordination, InMemoryStorage};

use super::Service;
use crate::domain::admission::Admission;
use crate::domain::bootstrap::Bound;
use crate::domain::error::{Dependency, DomainError};
use crate::domain::ports::metrics::NoopMetrics;
use crate::domain::readiness::Readiness;
use crate::test_support::{PermitTenantsPdp, tenant};

fn service() -> Service {
    let enforcer = PolicyEnforcer::new(Arc::new(PermitTenantsPdp::new(vec![tenant().as_uuid()])));
    Service::new(
        Admission::new(enforcer, Arc::new(NoopMetrics)),
        Arc::new(Readiness::new()),
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
        svc.coordination().err(),
        Some(DomainError::NotReady {
            dependency: Dependency::Coordination
        })
    );

    let bound = Bound {
        storage: Arc::new(InMemoryStorage::new()),
        coordination: Arc::new(InMemoryCoordination::new()),
    };
    svc.bind(bound.clone()).expect("first bind");
    assert!(svc.storage().is_ok());
    assert!(svc.coordination().is_ok());
    assert!(matches!(svc.bind(bound), Err(DomainError::Internal(_))));
    assert!(
        !svc.readiness().is_ready(),
        "binding does not imply readiness; bootstrap marks it"
    );
}
