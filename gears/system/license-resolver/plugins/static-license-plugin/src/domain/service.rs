//! Domain service for the static license resolver plugin.

use license_resolver_sdk::{LicenseCheckRequest, LicenseDecision};
use toolkit_macros::domain_model;

use crate::config::{GrantRule, StaticLicensePluginConfig};

/// Backend identifier reported in [`diagnostics::BACKEND`].
pub const BACKEND_ID: &str = "static-license-plugin";

/// Keys this backend puts in [`LicenseDecision::diagnostics`].
///
/// Advisory only: the `granted` boolean is authoritative on its own and none of
/// these are required to read it.
pub mod diagnostics {
    /// Which backend answered.
    pub const BACKEND: &str = "backend";
    /// Index of the grant rule that answered, present only on a grant.
    pub const MATCHED_RULE: &str = "matched_rule";
    /// Why nothing was granted, present only on a denial.
    pub const DENY_REASON: &str = "deny_reason";
}

/// Values this backend reports under [`diagnostics::DENY_REASON`].
pub mod deny_reason {
    /// The backend holds no rules at all, so it can never grant.
    pub const NO_GRANTS_CONFIGURED: &str = "no_grants_configured";
    /// Rules exist, but none covers this subject/resource pair.
    pub const NO_MATCHING_GRANT: &str = "no_matching_grant";
}

/// Static license resolver service.
///
/// Holds the configured grant rules in memory and answers a check by finding the
/// first rule that covers it. Deny by default: no rule, no grant.
#[domain_model]
pub struct Service {
    grants: Vec<GrantRule>,
}

impl Service {
    /// Builds the service from configuration.
    ///
    /// # Errors
    ///
    /// Propagates [`StaticLicensePluginConfig::validate`] errors.
    pub fn from_config(cfg: &StaticLicensePluginConfig) -> anyhow::Result<Self> {
        cfg.validate()?;
        Ok(Self {
            grants: cfg.grants.clone(),
        })
    }

    /// Answers a check from the configured rules.
    #[must_use]
    pub fn evaluate(&self, request: &LicenseCheckRequest) -> LicenseDecision {
        if self.grants.is_empty() {
            return denied(deny_reason::NO_GRANTS_CONFIGURED);
        }
        match self.grants.iter().position(|rule| rule.matches(request)) {
            Some(index) => LicenseDecision::new(true)
                .with_diagnostic(diagnostics::BACKEND, BACKEND_ID)
                .with_diagnostic(diagnostics::MATCHED_RULE, index),
            None => denied(deny_reason::NO_MATCHING_GRANT),
        }
    }
}

fn denied(reason: &'static str) -> LicenseDecision {
    LicenseDecision::new(false)
        .with_diagnostic(diagnostics::BACKEND, BACKEND_ID)
        .with_diagnostic(diagnostics::DENY_REASON, reason)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "service_tests.rs"]
mod service_tests;
