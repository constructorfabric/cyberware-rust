//! Client implementation for the static license resolver plugin.

use async_trait::async_trait;
use license_resolver_sdk::{
    LicenseCheckRequest, LicenseDecision, LicenseResolverError, LicenseResolverPluginClient,
};

use super::service::Service;

#[async_trait]
impl LicenseResolverPluginClient for Service {
    async fn is_licensed(
        &self,
        request: LicenseCheckRequest,
    ) -> Result<LicenseDecision, LicenseResolverError> {
        // Rules are in memory, so nothing here can be unreachable or refuse the
        // caller — a not-granted answer is always a decision, never an error.
        Ok(self.evaluate(&request))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "client_tests.rs"]
mod client_tests;
