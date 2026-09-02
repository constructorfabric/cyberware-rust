//! Readiness health check. `api-gateway` aggregates it into `/readyz` and
//! `/health`, so a failed bootstrap names its dependency to operators.

use std::sync::Arc;

use async_trait::async_trait;
use toolkit::{Healthcheck, HealthcheckResult};

use crate::domain::{Readiness, ReadinessState};

/// Check name shown in `/health`.
pub const CHECK_NAME: &str = "quota-enforcement-bootstrap";

/// Reports the bootstrap state.
pub struct ReadinessCheck {
    readiness: Arc<Readiness>,
}

impl ReadinessCheck {
    /// Wrap the shared readiness cell.
    #[must_use]
    pub fn new(readiness: Arc<Readiness>) -> Self {
        Self { readiness }
    }
}

#[async_trait]
impl Healthcheck for ReadinessCheck {
    fn name(&self) -> &'static str {
        CHECK_NAME
    }

    // @cpt-flow:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1
    async fn check(&self) -> HealthcheckResult {
        // @cpt-begin:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-probe-abort
        match self.readiness.snapshot() {
            ReadinessState::Ready => HealthcheckResult::healthy(),
            ReadinessState::Starting => HealthcheckResult::unhealthy("bootstrap in progress")
                .with_code("qe_bootstrap_pending"),
            ReadinessState::Failed { dependency, reason } => {
                HealthcheckResult::unhealthy(format!("{dependency} unavailable: {reason}"))
                    .with_code(format!("qe_{}_unavailable", dependency.as_label()))
            }
        }
        // @cpt-end:cpt-cf-quota-enforcement-flow-gear-bootstrap:p1:inst-boot-probe-abort
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "healthcheck_tests.rs"]
mod healthcheck_tests;
