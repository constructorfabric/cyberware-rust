//! Types-registry adapter behind the [`ContractRegistry`] port.
//!
//! The adapter caches nothing across calls: every check re-resolves through the
//! SDK so a contract update takes effect immediately. The types-registry local
//! client owns whatever short-lived caching it chooses to do internally.

use std::sync::Arc;

use async_trait::async_trait;
use toolkit::client_hub::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::{GtsTypeSchema, TypesRegistryClient};

use crate::domain::{ContractRegistry, ContractRegistryError};

/// Resolves licensing contracts through the types-registry client.
pub struct GtsContractRegistry {
    hub: Arc<ClientHub>,
}

impl GtsContractRegistry {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>) -> Self {
        Self { hub }
    }
}

#[async_trait]
impl ContractRegistry for GtsContractRegistry {
    async fn contract_schema(&self, type_id: &str) -> Result<GtsTypeSchema, ContractRegistryError> {
        // Resolved per call rather than captured during `init()`: types-registry
        // commits its catalog after the system-gear init phase, so a client
        // captured there would be held before any contract is resolvable.
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| ContractRegistryError::Unavailable(e.to_string()))?;

        registry
            .get_type_schema(type_id)
            .await
            .map_err(|err| match err {
                CanonicalError::NotFound { .. } => ContractRegistryError::Unregistered,
                // A malformed or kind-mismatched id is the caller's fault: the
                // registry reports it as InvalidArgument, not as an outage.
                CanonicalError::InvalidArgument { detail, .. } => {
                    ContractRegistryError::MalformedTypeId(detail)
                }
                other => ContractRegistryError::Unavailable(other.to_string()),
            })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "types_registry_tests.rs"]
mod types_registry_tests;
