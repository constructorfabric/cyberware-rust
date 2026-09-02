use std::sync::Arc;

use toolkit::{Healthcheck, HealthcheckResult};

use super::{CHECK_NAME, ReadinessCheck};
use crate::domain::error::Dependency;
use crate::domain::readiness::Readiness;

#[tokio::test]
async fn the_check_mirrors_the_readiness_cell_and_names_the_failing_dependency() {
    let readiness = Arc::new(Readiness::new());
    let check = ReadinessCheck::new(readiness.clone());
    assert_eq!(check.name(), CHECK_NAME);

    let pending = check.check().await;
    let unhealthy = HealthcheckResult::unhealthy("x").status;
    let healthy = HealthcheckResult::healthy().status;
    assert_eq!(pending.status, unhealthy);
    assert_eq!(pending.code.as_deref(), Some("qe_bootstrap_pending"));

    readiness.mark_failed(Dependency::Coordination, "probe failed on lease_sweeper");
    let failed = check.check().await;
    assert_eq!(failed.status, unhealthy);
    assert_eq!(failed.code.as_deref(), Some("qe_coordination_unavailable"));
    let message = failed.message.expect("failure message");
    assert!(message.contains("coordination"), "{message}");
    assert!(message.contains("lease_sweeper"), "{message}");

    readiness.mark_ready();
    let ready = check.check().await;
    assert_eq!(ready.status, healthy);
    assert!(ready.code.is_none());
}
