//! Plugin API trait for license resolver backend implementations.

use async_trait::async_trait;

use crate::error::LicenseResolverError;
use crate::models::{LicenseCheckRequest, LicenseDecision};

/// Plugin API trait for license resolver backends.
///
/// Mirrors [`LicenseResolverClient`](crate::api::LicenseResolverClient) with
/// the same single `is_licensed` method. Each plugin registers this trait as a
/// scoped `ClientHub` entry keyed by its GTS instance id, and is discovered by
/// the gateway via the [`LicenseResolverPluginSpecV1`](crate::gts::LicenseResolverPluginSpecV1)
/// spec (vendor + priority). It tracks the public contract's major version.
///
/// The gateway only delegates requests that already conform to a registered
/// contract; the plugin owns the catalog of licensable types and the grant
/// semantics, and MAY use the forwarded `metadata` for attribute-based
/// licensing.
#[async_trait]
pub trait LicenseResolverPluginClient: Send + Sync {
    /// Answer the delegated license check for the given subject within the
    /// tenant scope carried in `request.context`.
    ///
    /// # Errors
    ///
    /// - [`Unauthorized`](LicenseResolverError::Unauthorized) — the backend
    ///   refused to answer for this caller/subject. The gateway cannot originate it,
    ///   having no caller identity of its own, and propagates it unchanged.
    ///   **Not** the way to say "not licensed" — that is `LicenseDecision { granted: false }`.
    /// - [`InvalidRequest`](LicenseResolverError::InvalidRequest) — the backend
    ///   holds a constraint the registered contract does not express, so the
    ///   request conforms to its contract yet cannot be answered as asked. Its
    ///   violations carry the backend's own `field` / `reason` vocabulary rather
    ///   than [`crate::field`]'s; the gateway propagates them unchanged and does
    ///   not label its own telemetry with them.
    /// - [`ServiceUnavailable`](LicenseResolverError::ServiceUnavailable) — backend unreachable/erroring.
    /// - [`Internal`](LicenseResolverError::Internal) — unexpected failure.
    async fn is_licensed(
        &self,
        request: LicenseCheckRequest,
    ) -> Result<LicenseDecision, LicenseResolverError>;
}
