//! Output port for the gear-specific instruments (PRD section 5.16).
//!
//! The foundation owns one catalogue instrument: `denial_total{reason}`,
//! the count of admission denials by closed reason. Later features add their
//! instruments to this port; none of them leaves the catalogue.
//!
//! Cardinality rule (`cpt-cf-quota-enforcement-constraint-bounded-cardinality`):
//! every label value is a `&'static str` from a closed enum. `tenant_id`,
//! `subject_id`, `quota_id`, `policy_id`, idempotency keys, lease tokens,
//! projection types, caller attribution, and raw metric input never appear as
//! labels. They belong to spans and structured logs.

use toolkit_macros::domain_model;

/// Label key of `denial_total`.
pub const REASON_LABEL: &str = "reason";

/// Closed `reason` set of `denial_total`.
// @cpt-dod:cpt-cf-quota-enforcement-dod-telemetry-conventions:p1
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialReason {
    /// The PDP denied, or its constraints did not compile.
    PermissionDenied,
    /// The PDP could not be reached. Fail closed.
    PdpUnavailable,
    /// The public request shape was invalid before the PDP call.
    InvalidArgument,
    /// Bootstrap has not completed.
    NotReady,
}

impl DenialReason {
    /// Every value, for conformance tests.
    pub const ALL: [Self; 4] = [
        Self::PermissionDenied,
        Self::PdpUnavailable,
        Self::InvalidArgument,
        Self::NotReady,
    ];

    /// Stable label value.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::PermissionDenied => "permission_denied",
            Self::PdpUnavailable => "pdp_unavailable",
            Self::InvalidArgument => "invalid_argument",
            Self::NotReady => "not_ready",
        }
    }
}

/// Gear-specific instruments. Implemented on the platform meter in
/// `infra::metrics`; a no-op double serves tests.
pub trait QeMetrics: Send + Sync {
    /// `denial_total{reason}` += 1.
    fn record_denial(&self, reason: DenialReason);
}

/// Records nothing. For tests and pre-init contexts.
#[domain_model]
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMetrics;

impl QeMetrics for NoopMetrics {
    fn record_denial(&self, _reason: DenialReason) {}
}
