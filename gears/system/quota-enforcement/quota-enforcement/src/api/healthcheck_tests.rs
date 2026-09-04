use std::sync::Arc;

use async_trait::async_trait;
use toolkit::{Healthcheck, HealthcheckResult};

use super::{CHECK_NAME, ReadinessCheck};
use crate::domain::error::Dependency;
use crate::domain::readiness::Readiness;

/// A cluster readiness contributor with a fixed verdict.
struct FixedCluster(HealthcheckResult);

#[async_trait]
impl Healthcheck for FixedCluster {
    fn name(&self) -> &'static str {
        "cluster-requirements"
    }

    async fn check(&self) -> HealthcheckResult {
        self.0.clone()
    }
}

#[tokio::test]
async fn the_check_mirrors_the_readiness_cell_and_names_the_failing_dependency() {
    let readiness = Arc::new(Readiness::new());
    let check = ReadinessCheck::new(readiness.clone(), None);
    assert_eq!(check.name(), CHECK_NAME);

    let pending = check.check().await;
    let unhealthy = HealthcheckResult::unhealthy("x").status;
    let healthy = HealthcheckResult::healthy().status;
    assert_eq!(pending.status, unhealthy);
    assert_eq!(pending.code.as_deref(), Some("qe_bootstrap_pending"));

    readiness.mark_failed(Dependency::Cluster, "no backend bound for profile");
    let failed = check.check().await;
    assert_eq!(failed.status, unhealthy);
    assert_eq!(failed.code.as_deref(), Some("qe_cluster_unavailable"));
    let message = failed.message.expect("failure message");
    assert!(message.contains("cluster"), "{message}");
    assert!(message.contains("profile"), "{message}");

    readiness.mark_ready();
    let ready = check.check().await;
    assert_eq!(ready.status, healthy);
    assert!(ready.code.is_none());
}

#[tokio::test]
async fn once_ready_the_check_relays_the_cluster_requirements_verdict() {
    let readiness = Arc::new(Readiness::new());
    readiness.mark_ready();
    let unhealthy = HealthcheckResult::unhealthy("x").status;
    let healthy = HealthcheckResult::healthy().status;

    let fine = ReadinessCheck::new(
        readiness.clone(),
        Some(Arc::new(FixedCluster(HealthcheckResult::healthy()))),
    );
    assert_eq!(fine.check().await.status, healthy);

    let broken = ReadinessCheck::new(
        readiness.clone(),
        Some(Arc::new(FixedCluster(
            HealthcheckResult::unhealthy("capability not met for `LeaderElectionV1`")
                .with_code("cluster_capability_not_met"),
        ))),
    );
    let result = broken.check().await;
    assert_eq!(result.status, unhealthy);
    assert_eq!(result.code.as_deref(), Some("qe_cluster_unavailable"));
    let message = result.message.expect("message");
    assert!(message.contains("cluster unavailable"), "{message}");
    assert!(message.contains("LeaderElectionV1"), "{message}");

    readiness.mark_failed(Dependency::Storage, "db down");
    let storage_first = broken.check().await;
    assert_eq!(
        storage_first.code.as_deref(),
        Some("qe_storage_unavailable"),
        "a bootstrap failure is reported before the cluster verdict"
    );
}
