//! Domain errors for the license resolver gear.

use license_resolver_sdk::{FieldViolation, LicenseResolverError};
use toolkit_macros::domain_model;

/// Internal domain errors.
#[domain_model]
#[derive(thiserror::Error, Debug)]
pub enum DomainError {
    #[error("types registry is not available: {0}")]
    TypesRegistryUnavailable(String),

    #[error("no plugin instances found for vendor '{vendor}'")]
    PluginNotFound { vendor: String },

    #[error("invalid plugin instance content for '{gts_id}': {reason}")]
    InvalidPluginInstance { gts_id: String, reason: String },

    #[error("plugin not available for '{gts_id}': {reason}")]
    PluginUnavailable { gts_id: String, reason: String },

    /// The request does not conform to the licensing contracts it declares.
    /// Carries every violation found, not only the first.
    #[error("request does not conform to its licensing contracts: {} violation(s)", violations.len())]
    ContractViolation { violations: Vec<FieldViolation> },

    /// A registered contract cannot be used to judge a request — its schema does
    /// not compile, or a trait it declares has the wrong shape. Catalog drift:
    /// an operator has to fix the registered type, so it must not be reported as
    /// the caller's fault.
    #[error("registered contract '{type_id}' is unusable: {reason}")]
    ContractUnusable { type_id: String, reason: String },

    /// The backend refused to answer for this caller. Originates in a plugin
    /// fronting a licensing service that polices the query itself; the gateway
    /// holds no caller identity and never raises it.
    #[error("unauthorized")]
    Unauthorized,

    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    /// Bring an error surfaced by the selected backend into the domain, keeping
    /// the instance id that produced it.
    #[must_use]
    pub fn from_plugin(instance_id: &str, err: LicenseResolverError) -> Self {
        match err {
            LicenseResolverError::Unauthorized => Self::Unauthorized,
            LicenseResolverError::InvalidRequest { violations } => {
                Self::ContractViolation { violations }
            }
            LicenseResolverError::ServiceUnavailable(reason) => Self::PluginUnavailable {
                gts_id: instance_id.to_owned(),
                reason,
            },
            // A backend has no plugins of its own to be missing, so this is a
            // protocol violation rather than this gateway's no-plugin state.
            LicenseResolverError::NoPluginAvailable => Self::Internal(format!(
                "plugin '{instance_id}' reported NoPluginAvailable, which only the gateway can raise"
            )),
            LicenseResolverError::Internal(reason) => Self::Internal(reason),
            other => Self::Internal(format!("plugin '{instance_id}' failed: {other}")),
        }
    }
}

impl From<toolkit::plugins::ChoosePluginError> for DomainError {
    fn from(e: toolkit::plugins::ChoosePluginError) -> Self {
        match e {
            toolkit::plugins::ChoosePluginError::InvalidPluginInstance { gts_id, reason } => {
                Self::InvalidPluginInstance { gts_id, reason }
            }
            toolkit::plugins::ChoosePluginError::PluginNotFound { vendor, .. } => {
                Self::PluginNotFound { vendor }
            }
        }
    }
}

impl From<DomainError> for LicenseResolverError {
    fn from(e: DomainError) -> Self {
        match e {
            DomainError::ContractViolation { violations } => Self::InvalidRequest { violations },
            DomainError::PluginNotFound { .. } => Self::NoPluginAvailable,
            DomainError::InvalidPluginInstance { gts_id, reason } => {
                Self::Internal(format!("invalid plugin instance '{gts_id}': {reason}"))
            }
            DomainError::PluginUnavailable { gts_id, reason } => {
                Self::ServiceUnavailable(format!("plugin not available for '{gts_id}': {reason}"))
            }
            // Cannot-determine, so it fails closed as unavailable rather than as
            // an internal fault: the catalog the check needs is out of reach.
            DomainError::TypesRegistryUnavailable(reason) => {
                Self::ServiceUnavailable(format!("types registry not available: {reason}"))
            }
            DomainError::ContractUnusable { type_id, reason } => Self::Internal(format!(
                "registered contract '{type_id}' is unusable: {reason}"
            )),
            DomainError::Unauthorized => Self::Unauthorized,
            DomainError::Internal(reason) => Self::Internal(reason),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_tests.rs"]
mod error_tests;
