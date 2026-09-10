//! Domain ports — the abstractions the resolver's logic depends on.

use async_trait::async_trait;
use license_resolver_sdk::field;
use toolkit_macros::domain_model;
use types_registry_sdk::GtsTypeSchema;

use super::error::DomainError;

/// Why a licensing contract could not be resolved.
///
/// The split is load-bearing: the first two are the caller's fault and become
/// request violations, the third is a cannot-determine condition the check must
/// fail closed on. Collapsing them would report a registry outage as a bad
/// request, or a caller's typo as an outage.
#[domain_model]
#[derive(thiserror::Error, Debug)]
pub enum ContractRegistryError {
    /// No type-schema is registered under this id.
    #[error("contract type is not registered")]
    Unregistered,

    /// The id is not a well-formed GTS type id.
    #[error("contract type is not a well-formed GTS type id: {0}")]
    MalformedTypeId(String),

    /// The registry could not be reached, or failed for any other reason.
    #[error("types registry is not available: {0}")]
    Unavailable(String),
}

/// Resolves registered licensing contract type-schemas.
///
/// Deliberately narrower than the types-registry client: the validation pipeline
/// needs one lookup, and the adapter behind this port owns the classification of
/// registry failures into [`ContractRegistryError`].
#[async_trait]
pub trait ContractRegistry: Send + Sync {
    /// Resolve a contract type-schema with its derivation chain linked, so
    /// `ancestors` / `effective_*` observe the whole chain.
    ///
    /// # Errors
    ///
    /// - [`ContractRegistryError::Unregistered`] — nothing registered under `type_id`.
    /// - [`ContractRegistryError::MalformedTypeId`] — `type_id` is not a GTS type id.
    /// - [`ContractRegistryError::Unavailable`] — the registry could not answer.
    async fn contract_schema(&self, type_id: &str) -> Result<GtsTypeSchema, ContractRegistryError>;
}

/// How a check ended. A bounded set, because it is a metric label.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The backend answered `granted: true`.
    Granted,
    /// The backend answered `granted: false`.
    NotGranted,
    /// The request did not conform to its licensing contracts.
    InvalidRequest,
    /// No backend plugin is registered for the configured vendor.
    NoPlugin,
    /// The backend, or the registry the check depends on, was unreachable.
    Unavailable,
    /// The backend refused to answer for this caller.
    Unauthorized,
    /// Any other failure, including a registered contract that cannot be used.
    Error,
}

impl CheckOutcome {
    #[must_use]
    pub fn as_label(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::NotGranted => "not_granted",
            Self::InvalidRequest => "invalid_request",
            Self::NoPlugin => "no_plugin",
            Self::Unavailable => "unavailable",
            Self::Unauthorized => "unauthorized",
            Self::Error => "error",
        }
    }
}

impl From<&DomainError> for CheckOutcome {
    fn from(err: &DomainError) -> Self {
        match err {
            DomainError::ContractViolation { .. } => Self::InvalidRequest,
            DomainError::PluginNotFound { .. } => Self::NoPlugin,
            DomainError::TypesRegistryUnavailable(_) | DomainError::PluginUnavailable { .. } => {
                Self::Unavailable
            }
            DomainError::Unauthorized => Self::Unauthorized,
            DomainError::InvalidPluginInstance { .. }
            | DomainError::ContractUnusable { .. }
            | DomainError::Internal(_) => Self::Error,
        }
    }
}

/// Why a request was rejected. A bounded set, because it is a metric label.
///
/// The resolver's own violation vocabulary plus [`Other`](Self::Other), which
/// absorbs every `reason` raised outside it — a backend's own code, or one a
/// newer SDK adds. Without that collapse a backend could widen the label space
/// per request, since it is free to put request content in `reason`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationKind {
    /// [`field::CONTRACT_NOT_REGISTERED`].
    ContractNotRegistered,
    /// [`field::CONTRACT_TYPE_MALFORMED`].
    ContractTypeMalformed,
    /// [`field::CONTRACT_NOT_DERIVED`].
    ContractNotDerived,
    /// [`field::CONTRACT_ABSTRACT`].
    ContractAbstract,
    /// [`field::SCHEMA_MISMATCH`].
    SchemaMismatch,
    /// [`field::SUBJECT_NOT_ADMITTED`].
    SubjectNotAdmitted,
    /// Any reason outside the resolver's vocabulary.
    Other,
}

impl ViolationKind {
    #[must_use]
    pub fn as_label(self) -> &'static str {
        match self {
            Self::ContractNotRegistered => field::CONTRACT_NOT_REGISTERED,
            Self::ContractTypeMalformed => field::CONTRACT_TYPE_MALFORMED,
            Self::ContractNotDerived => field::CONTRACT_NOT_DERIVED,
            Self::ContractAbstract => field::CONTRACT_ABSTRACT,
            Self::SchemaMismatch => field::SCHEMA_MISMATCH,
            Self::SubjectNotAdmitted => field::SUBJECT_NOT_ADMITTED,
            Self::Other => "other",
        }
    }
}

impl From<&str> for ViolationKind {
    fn from(reason: &str) -> Self {
        match reason {
            field::CONTRACT_NOT_REGISTERED => Self::ContractNotRegistered,
            field::CONTRACT_TYPE_MALFORMED => Self::ContractTypeMalformed,
            field::CONTRACT_NOT_DERIVED => Self::ContractNotDerived,
            field::CONTRACT_ABSTRACT => Self::ContractAbstract,
            field::SCHEMA_MISMATCH => Self::SchemaMismatch,
            field::SUBJECT_NOT_ADMITTED => Self::SubjectNotAdmitted,
            _ => Self::Other,
        }
    }
}

/// Telemetry sink for the resolver's own surface.
///
/// Every label must be bounded: never an instance id, never a `metadata` value,
/// and never an unvalidated caller-supplied contract type. Unbounded values
/// belong in spans and logs, which are sampled and filter-controlled.
pub trait LicenseMetrics: Send + Sync {
    fn record_check(&self, contract_type: &str, outcome: CheckOutcome);

    /// Resolver-side latency: contract validation plus plugin selection,
    /// excluding the delegated call. Label-free: the latency target covers the
    /// whole surface, and a per-contract breakdown lives in the span.
    fn record_resolver_latency(&self, millis: f64);

    /// One contract violation.
    fn record_validation_failure(&self, kind: ViolationKind);
}

/// Metrics sink that records nothing.
#[domain_model]
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMetrics;

impl LicenseMetrics for NoopMetrics {
    fn record_check(&self, _contract_type: &str, _outcome: CheckOutcome) {}
    fn record_resolver_latency(&self, _millis: f64) {}
    fn record_validation_failure(&self, _kind: ViolationKind) {}
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "ports_tests.rs"]
mod ports_tests;
