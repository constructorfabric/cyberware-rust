//! Local (in-process) client for the license resolver gear.

use std::sync::Arc;

use async_trait::async_trait;
use license_resolver_sdk::{
    LicenseCheckRequest, LicenseDecision, LicenseResolverClient, LicenseResolverError,
};
use toolkit_macros::domain_model;

use super::Service;

/// Local client wrapping the license resolver service.
///
/// Registered in `ClientHub` by the gear during `init()`.
#[domain_model]
pub struct LicenseResolverLocalClient {
    svc: Arc<Service>,
}

impl LicenseResolverLocalClient {
    #[must_use]
    pub fn new(svc: Arc<Service>) -> Self {
        Self { svc }
    }
}

#[async_trait]
impl LicenseResolverClient for LicenseResolverLocalClient {
    async fn is_licensed(
        &self,
        request: LicenseCheckRequest,
    ) -> Result<LicenseDecision, LicenseResolverError> {
        self.svc
            .is_licensed(request)
            .await
            .map_err(LicenseResolverError::from)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "local_client_tests.rs"]
mod local_client_tests;
