//! Output port for the gear-specific instruments (PRD section 5.16).
//!
//! The foundation owns `denial_total{reason}`, the count of admission denials
//! by closed reason. The projection-contracts feature adds
//! `contract_validation_failures_total{surface, reason}` and
//! `admitted_metric_violations_total{surface}`. Later features add their
//! instruments to this port; none of them leaves the catalogue.
//!
//! Cardinality rule (`cpt-cf-quota-enforcement-constraint-bounded-cardinality`):
//! every label value is a `&'static str` from a closed enum. `tenant_id`,
//! `subject_id`, `quota_id`, `policy_id`, idempotency keys, lease tokens,
//! projection types, caller attribution, and raw metric input never appear as
//! labels. They belong to spans and structured logs.

use std::fmt;

use toolkit_macros::domain_model;

/// Label key of `denial_total` and of `contract_validation_failures_total`.
pub const REASON_LABEL: &str = "reason";

/// Label key of the contract-validation counters.
pub const SURFACE_LABEL: &str = "surface";

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

/// Closed `surface` set of the contract-validation counters (DESIGN section
/// 5.16): where a contract or admitted-metric check ran.
// @cpt-dod:cpt-cf-quota-enforcement-dod-contract-validation-telemetry:p1
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValidationSurface {
    /// Subject mapping and operation metadata at evaluation ingress.
    RequestSubject,
    /// The optional resource projection at evaluation ingress.
    RequestResource,
    /// The public shape of the caller-supplied attribution, before the PDP.
    CallerAttribution,
    /// A Quota write's arbitration metadata or projection reference.
    Arbitration,
    /// A Policy write's contract references.
    PolicyPair,
    /// The catalogue consistency set at bootstrap.
    Bootstrap,
}

impl ValidationSurface {
    /// Every value, for conformance tests.
    pub const ALL: [Self; 6] = [
        Self::RequestSubject,
        Self::RequestResource,
        Self::CallerAttribution,
        Self::Arbitration,
        Self::PolicyPair,
        Self::Bootstrap,
    ];

    /// Stable label value.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::RequestSubject => "request_subject",
            Self::RequestResource => "request_resource",
            Self::CallerAttribution => "caller_attribution",
            Self::Arbitration => "arbitration",
            Self::PolicyPair => "policy_pair",
            Self::Bootstrap => "bootstrap",
        }
    }
}

impl fmt::Display for ValidationSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_label())
    }
}

/// Closed `reason` set of `contract_validation_failures_total`: why a
/// contract instance or a catalogue entry was rejected. Also the reason a
/// bootstrap consistency check names in [`crate::domain::error::DomainError`].
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValidationReason {
    /// The public request shape was malformed (missing field, empty id,
    /// duplicate kind, repeated tenant scope, unparsable id).
    ShapeInvalid,
    /// The required metadata object was absent.
    MetadataMissing,
    /// The subject kind is not a scope the catalogue knows.
    KindUnknown,
    /// The subject kind is known but does not admit the metric.
    KindNotAdmitted,
    /// A metadata or resource document violated its contract schema.
    SchemaViolation,
    /// A resolved contract schema could not be compiled.
    SchemaInvalid,
    /// A referenced type is not registered.
    Unregistered,
    /// A referenced type is abstract where a concrete one is required.
    Abstract,
    /// A referenced type does not derive from the required QE base.
    NotDerived,
    /// A projection's `scope` trait is not a registered scope instance.
    ScopeInvalid,
    /// An admitted metric is not registered.
    MetricUnregistered,
    /// An admitted metric is registered but not an instance of the metric base.
    MetricNotInstance,
    /// Two configured projections admit the same `(metric, scope)` pair.
    DuplicatePair,
    /// An admitted metric has no concrete request contract.
    RequestContractMissing,
    /// An admitted metric has more than one concrete request contract.
    RequestContractAmbiguous,
    /// A request contract's attached constraint contract is unusable.
    ConstraintInvalid,
    /// A QE-owned definition exists in the registry with different content.
    DefinitionConflict,
    /// A projection is registered but outside the configured catalogue.
    ProjectionNotResolvable,
    /// The configured catalogue is incompatible with an active Quota.
    IncompatibleState,
}

impl ValidationReason {
    /// Every value, for conformance tests.
    pub const ALL: [Self; 19] = [
        Self::ShapeInvalid,
        Self::MetadataMissing,
        Self::KindUnknown,
        Self::KindNotAdmitted,
        Self::SchemaViolation,
        Self::SchemaInvalid,
        Self::Unregistered,
        Self::Abstract,
        Self::NotDerived,
        Self::ScopeInvalid,
        Self::MetricUnregistered,
        Self::MetricNotInstance,
        Self::DuplicatePair,
        Self::RequestContractMissing,
        Self::RequestContractAmbiguous,
        Self::ConstraintInvalid,
        Self::DefinitionConflict,
        Self::ProjectionNotResolvable,
        Self::IncompatibleState,
    ];

    /// Stable label value.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::ShapeInvalid => "shape_invalid",
            Self::MetadataMissing => "metadata_missing",
            Self::KindUnknown => "kind_unknown",
            Self::KindNotAdmitted => "kind_not_admitted",
            Self::SchemaViolation => "schema_violation",
            Self::SchemaInvalid => "schema_invalid",
            Self::Unregistered => "unregistered",
            Self::Abstract => "abstract",
            Self::NotDerived => "not_derived",
            Self::ScopeInvalid => "scope_invalid",
            Self::MetricUnregistered => "metric_unregistered",
            Self::MetricNotInstance => "metric_not_instance",
            Self::DuplicatePair => "duplicate_pair",
            Self::RequestContractMissing => "request_contract_missing",
            Self::RequestContractAmbiguous => "request_contract_ambiguous",
            Self::ConstraintInvalid => "constraint_invalid",
            Self::DefinitionConflict => "definition_conflict",
            Self::ProjectionNotResolvable => "projection_not_resolvable",
            Self::IncompatibleState => "incompatible_state",
        }
    }
}

impl fmt::Display for ValidationReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_label())
    }
}

/// Gear-specific instruments. Implemented on the platform meter in
/// `infra::metrics`; a no-op double serves tests.
pub trait QeMetrics: Send + Sync {
    /// `denial_total{reason}` += 1.
    fn record_denial(&self, reason: DenialReason);

    /// `contract_validation_failures_total{surface, reason}` += 1.
    fn record_contract_validation_failure(
        &self,
        surface: ValidationSurface,
        reason: ValidationReason,
    );

    /// `admitted_metric_violations_total{surface}` += 1.
    fn record_admitted_metric_violation(&self, surface: ValidationSurface);
}

/// Records nothing. For tests and pre-init contexts.
#[domain_model]
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMetrics;

impl QeMetrics for NoopMetrics {
    fn record_denial(&self, _reason: DenialReason) {}

    fn record_contract_validation_failure(
        &self,
        _surface: ValidationSurface,
        _reason: ValidationReason,
    ) {
    }

    fn record_admitted_metric_violation(&self, _surface: ValidationSurface) {}
}
