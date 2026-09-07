//! `PdpReachability`: the bootstrap probe of the `authz-resolver` PDP.
//!
//! The `AuthZResolverApi` contract has one method, `evaluate`, and no health
//! check, so reachability is one bounded evaluation round trip. The request
//! names a fixed probe principal against the gear's own quota resource type
//! and requires no constraints. Both PDP plugins deny a principal they cannot
//! place in a tenant, and that denial is the expected answer: any decision
//! proves the PDP answered, while a transport error or the deadline proves it
//! did not. Registration of the client in the hub proves neither, which is why
//! `init` checking the registration is not enough.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::pep::enforcer::DEFAULT_EVAL_DEADLINE;
use authz_resolver_sdk::{
    Action, AuthZResolverApi, EvaluationRequest, EvaluationRequestContext, Resource, Subject,
};
use toolkit_security::PlatformSecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::pep::{actions, resources};
use crate::domain::ports::pdp::PdpProbe;

const LOG_TARGET: &str = "qe.bootstrap";

/// The principal every probe names. Fixed, so PDP audit logs attribute every
/// probe to one identity and never to a real caller.
pub const PROBE_SUBJECT: Uuid = Uuid::from_u128(0x0e00_0000_0000_4000_8000_0000_0000_9e0b);

/// The adapter over the PDP client `init` resolved from the hub.
pub struct PdpReachability {
    authz: Arc<dyn AuthZResolverApi>,
    deadline: Duration,
}

impl PdpReachability {
    /// Probe through `authz` with the PEP's default evaluation deadline.
    #[must_use]
    pub fn new(authz: Arc<dyn AuthZResolverApi>) -> Self {
        Self {
            authz,
            deadline: DEFAULT_EVAL_DEADLINE,
        }
    }

    /// Override the budget one probe may take.
    #[must_use]
    pub const fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }
}

/// The probe request: the shape `PolicyEnforcer` builds for a real `get`,
/// for a principal with no tenant.
fn probe_request() -> EvaluationRequest {
    EvaluationRequest {
        subject: Subject {
            id: PROBE_SUBJECT,
            subject_type: Some("service".to_owned()),
            properties: HashMap::new(),
        },
        action: Action {
            name: actions::GET.to_owned(),
        },
        resource: Resource {
            resource_type: resources::QUOTA.name().to_owned(),
            id: None,
            properties: HashMap::new(),
        },
        context: EvaluationRequestContext {
            tenant_context: None,
            token_scopes: Vec::new(),
            require_constraints: false,
            capabilities: Vec::new(),
            supported_properties: resources::QUOTA
                .supported_properties()
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            bearer_token: None,
        },
    }
}

#[async_trait]
impl PdpProbe for PdpReachability {
    async fn probe(&self) -> Result<(), DomainError> {
        // `evaluate` is a platform-plane method: the transport attaches the
        // gear's own service credential below the contract, so the marker
        // carries no identity (the same call shape `PolicyEnforcer` uses).
        let outcome = tokio::time::timeout(
            self.deadline,
            self.authz
                .evaluate(PlatformSecurityContext::outbound_marker(), probe_request()),
        )
        .await;
        match outcome {
            Ok(Ok(response)) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    decision = response.decision,
                    "the PDP answered the bootstrap probe"
                );
                Ok(())
            }
            Ok(Err(err)) => Err(DomainError::PdpUnavailable(err.to_string())),
            Err(_elapsed) => Err(DomainError::PdpUnavailable(format!(
                "authz-resolver did not answer the bootstrap probe within {:?}",
                self.deadline
            ))),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "pdp_probe_tests.rs"]
mod pdp_probe_tests;
