//! Public API trait for the license resolver.

use async_trait::async_trait;

use crate::error::LicenseResolverError;
use crate::models::{LicenseCheckRequest, LicenseDecision};

/// Public API trait for the license resolver gateway.
///
/// Registered in `ClientHub` by the gear and consumed by other gears that gate
/// access to a licensable resource.
#[async_trait]
pub trait LicenseResolverClient: Send + Sync {
    /// Point-in-time check of whether a resource is licensed to a subject.
    ///
    /// The resolver validates the request against the registered contracts,
    /// then delegates to the selected backend plugin.
    ///
    /// A not-granted answer is returned as `LicenseDecision { granted: false }`,
    /// **not** as an error.
    ///
    /// # Errors
    ///
    /// - [`Unauthorized`](LicenseResolverError::Unauthorized) — caller/subject not permitted.
    /// - [`InvalidRequest`](LicenseResolverError::InvalidRequest) — the request does not conform to its contracts.
    /// - [`NoPluginAvailable`](LicenseResolverError::NoPluginAvailable) — no backend plugin registered.
    /// - [`ServiceUnavailable`](LicenseResolverError::ServiceUnavailable) — backend/registry unreachable.
    /// - [`Internal`](LicenseResolverError::Internal) — unexpected failure.
    async fn is_licensed(
        &self,
        request: LicenseCheckRequest,
    ) -> Result<LicenseDecision, LicenseResolverError>;
}
