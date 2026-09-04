//! Closed domain error of the gear and its lifts from dependency errors.
//!
//! Chain: `StorageError -> DomainError -> CanonicalError`. The canonical lift
//! lives in `infra::canonical_mapping`; this module stays free of transport
//! and database types.

use std::fmt;

use authz_resolver_sdk::EnforcerError;
use quota_enforcement_sdk::{LeaseToken, PolicyId, StorageError};
use toolkit::plugins::ChoosePluginError;
use toolkit_macros::domain_model;

/// Which plugin family a binding error is about.
///
/// Singleton coordination is not a plugin family: the gear consumes the
/// platform `cluster` gear's leader election (ADR-0006).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    /// `QuotaEnforcementStoragePluginV1`.
    Storage,
}

impl fmt::Display for PluginKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Storage => "storage",
        })
    }
}

/// A bootstrap dependency, as surfaced in the health endpoint.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dependency {
    /// The storage plugin.
    Storage,
    /// The platform `cluster` gear (sweeper leader election).
    Cluster,
    /// The PDP (`authz-resolver`).
    Pdp,
    /// The types registry.
    TypesRegistry,
}

impl Dependency {
    /// Stable lowercase label for health codes and logs.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            Self::Cluster => "cluster",
            Self::Pdp => "pdp",
            Self::TypesRegistry => "types_registry",
        }
    }
}

impl fmt::Display for Dependency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_label())
    }
}

/// Resource family of a not-found error. Selects the GTS resource type of
/// the canonical envelope.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// A Quota record.
    Quota,
    /// A Quota Resolution Policy.
    Policy,
    /// A two-phase lease.
    Lease,
    /// An operation-log record.
    Operation,
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Quota => "quota",
            Self::Policy => "policy",
            Self::Lease => "lease",
            Self::Operation => "operation",
        })
    }
}

/// Authoritative business-error surface of the gear.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    // --- admission ---
    /// The PDP denied the operation, or its constraints could not be compiled.
    #[error("permission denied{}", reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default())]
    PdpDenied {
        /// Closed reason token or PDP-supplied code. Operator-facing only.
        reason: Option<String>,
    },
    /// The PDP could not be reached. Fail closed.
    #[error("authorization service unavailable: {0}")]
    PdpUnavailable(String),
    /// A public request-shape violation, found before the PDP call.
    #[error("invalid argument {field}: {reason}")]
    InvalidArgument {
        /// The request field.
        field: &'static str,
        /// Closed `UPPER_SNAKE` reason token.
        reason: &'static str,
    },

    // --- readiness and binding ---
    /// Bootstrap has not completed; the dependency is not usable yet.
    #[error("quota enforcement is not ready: {dependency} is not bound")]
    NotReady {
        /// The unbound dependency.
        dependency: Dependency,
    },
    /// The types registry could not answer.
    #[error("types registry unavailable: {0}")]
    TypesRegistryUnavailable(String),
    /// No plugin instance matched the configured vendor.
    #[error("no {kind} plugin instance registered for vendor {vendor}")]
    PluginNotFound {
        /// Plugin family.
        kind: PluginKind,
        /// Configured vendor.
        vendor: String,
    },
    /// A registered plugin instance did not deserialize as a plugin spec.
    #[error("invalid {kind} plugin instance {gts_id}: {reason}")]
    InvalidPluginInstance {
        /// Plugin family.
        kind: PluginKind,
        /// The instance id.
        gts_id: String,
        /// Deserialization detail.
        reason: String,
    },
    /// The instance is registered but its scoped client is not.
    #[error("{kind} plugin client for {gts_id} is not registered")]
    PluginClientNotRegistered {
        /// Plugin family.
        kind: PluginKind,
        /// The instance id.
        gts_id: String,
    },
    /// The platform `cluster` gear cannot provide the sweeper election: the
    /// `quota-enforcement` profile is unbound, its backend has no linearizable
    /// election, or the election closed.
    #[error("cluster unavailable: {0}")]
    ClusterUnavailable(String),
    /// Storage schema major differs from the contract major (I12).
    #[error("storage schema major {installed} does not match contract major {expected}")]
    SchemaVersionMismatch {
        /// Installed major.
        installed: u32,
        /// Expected major.
        expected: u32,
    },
    /// The storage backend could not be reached.
    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),

    // --- lookups ---
    /// The named resource does not exist in the caller's scope.
    #[error("{kind} {id} not found")]
    NotFound {
        /// Resource family.
        kind: ResourceKind,
        /// Identifier.
        id: String,
    },

    // --- 1:1 lifts of `StorageError` ---
    /// Commit or release against a non-active lease.
    #[error("lease {token} is not active")]
    LeaseNotActive {
        /// The lease token.
        token: LeaseToken,
    },
    /// The per-`(tenant, metric)` active-lease cap is reached.
    #[error("active-lease cap reached for the (tenant, metric) pair")]
    LeaseInflightLimitExceeded,
    /// The acquisition contention timeout elapsed.
    #[error("acquisition contention timeout elapsed")]
    LeaseContentionTimeout,
    /// Commit amount exceeds the reserved amount.
    #[error("commit amount {actual} exceeds reserved amount {reserved}")]
    OverCommitNotAuthorized {
        /// Reserved at acquisition.
        reserved: u64,
        /// Requested at commit.
        actual: u64,
    },
    /// Same idempotency scope, different payload.
    #[error("idempotency key replayed with a different payload")]
    IdempotencyPayloadMismatch,
    /// Optimistic-concurrency version mismatch.
    #[error("version conflict: expected {expected}, found {actual}")]
    VersionConflict {
        /// Expected version.
        expected: u32,
        /// Found version.
        actual: u32,
    },
    /// The named policy version does not exist.
    #[error("unknown version {version} of policy {policy_id}")]
    UnknownPolicyVersion {
        /// The policy.
        policy_id: PolicyId,
        /// The version.
        version: u32,
    },
    /// Rollback target is terminal.
    #[error("version {version} of policy {policy_id} was rolled back")]
    VersionRolledBack {
        /// The policy.
        policy_id: PolicyId,
        /// The version.
        version: u32,
    },
    /// Cap reduction below the consumed amount.
    #[error("cap {new_cap} is below the consumed amount {consumed}")]
    CapBelowConsumed {
        /// Requested cap.
        new_cap: u64,
        /// Consumed amount.
        consumed: u64,
    },
    /// The Quota is deactivated.
    #[error("quota {id} is deactivated")]
    QuotaDeactivated {
        /// The Quota.
        id: String,
    },
    /// The target period is closed.
    #[error("the target period is closed")]
    PeriodClosed,
    /// The metric is not registered.
    #[error("metric {metric} is not registered")]
    MetricNotRegistered {
        /// The metric.
        metric: String,
    },
    /// The metric is not quota-gated.
    #[error("metric {metric} is not quota-gated")]
    MetricNotQuotaGated {
        /// The metric.
        metric: String,
    },
    /// The projection is not registered.
    #[error("projection {projection} is not registered")]
    ProjectionNotRegistered {
        /// The projection type.
        projection: String,
    },

    /// Last-resort opaque failure. Never carries caller-facing detail.
    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    /// Closed reason token for the `PdpDenied` variant when the gateway's own
    /// post-permit gate rejects the target tenant.
    pub const TENANT_OUT_OF_SCOPE: &'static str = "TENANT_OUT_OF_SCOPE";
    /// Closed reason token when the PDP permit carried no usable constraints.
    pub const CONSTRAINT_COMPILE_FAILED: &'static str = "CONSTRAINT_COMPILE_FAILED";
    /// Closed reason token when storage caught a subject outside the scope.
    pub const SUBJECT_OUT_OF_SCOPE: &'static str = "SUBJECT_OUT_OF_SCOPE";

    /// Lift a plugin-selection failure for one plugin family.
    #[must_use]
    pub fn plugin_selection(kind: PluginKind, err: ChoosePluginError) -> Self {
        match err {
            ChoosePluginError::InvalidPluginInstance { gts_id, reason } => {
                Self::InvalidPluginInstance {
                    kind,
                    gts_id,
                    reason,
                }
            }
            ChoosePluginError::PluginNotFound { vendor, .. } => {
                Self::PluginNotFound { kind, vendor }
            }
        }
    }
}

impl DomainError {
    /// Fail-closed lift of the PEP outcome: a denial and a compile failure are
    /// both denials; only a transport failure is an availability error.
    ///
    /// A named constructor rather than `From`: the domain error is `Clone + Eq`
    /// and crosses the layer boundary as a value, so the transport cause is
    /// kept as text here and logged in full at the admission site.
    #[must_use]
    pub fn from_enforcer(err: EnforcerError) -> Self {
        match err {
            EnforcerError::Denied { deny_reason } => Self::PdpDenied {
                reason: deny_reason.map(|r| r.error_code),
            },
            EnforcerError::CompileFailed(_) => Self::PdpDenied {
                reason: Some(Self::CONSTRAINT_COMPILE_FAILED.to_owned()),
            },
            EnforcerError::EvaluationFailed(cause) => Self::PdpUnavailable(cause.to_string()),
        }
    }
}

/// 1:1 lift of the storage contract errors (DESIGN section 3.3).
impl From<StorageError> for DomainError {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::LeaseNotActive { token } => Self::LeaseNotActive { token },
            StorageError::LeaseInflightLimitExceeded => Self::LeaseInflightLimitExceeded,
            StorageError::LeaseContentionTimeout => Self::LeaseContentionTimeout,
            StorageError::OverCommitNotAuthorized { reserved, actual } => {
                Self::OverCommitNotAuthorized { reserved, actual }
            }
            StorageError::IdempotencyPayloadMismatch => Self::IdempotencyPayloadMismatch,
            StorageError::VersionConflict { expected, actual } => {
                Self::VersionConflict { expected, actual }
            }
            StorageError::UnknownPolicyVersion { policy_id, version } => {
                Self::UnknownPolicyVersion { policy_id, version }
            }
            StorageError::VersionRolledBack { policy_id, version } => {
                Self::VersionRolledBack { policy_id, version }
            }
            StorageError::CapBelowConsumed { new_cap, consumed } => {
                Self::CapBelowConsumed { new_cap, consumed }
            }
            StorageError::QuotaNotFound { id } => Self::NotFound {
                kind: ResourceKind::Quota,
                id: id.to_string(),
            },
            StorageError::QuotaDeactivated { id } => Self::QuotaDeactivated { id: id.to_string() },
            StorageError::PeriodClosed => Self::PeriodClosed,
            StorageError::MetricNotRegistered { metric } => Self::MetricNotRegistered { metric },
            StorageError::MetricNotQuotaGated { metric } => Self::MetricNotQuotaGated { metric },
            StorageError::ProjectionNotRegistered { projection } => {
                Self::ProjectionNotRegistered { projection }
            }
            // Storage-layer defense in depth caught what the PDP should have.
            StorageError::SubjectOutOfScope => Self::PdpDenied {
                reason: Some(Self::SUBJECT_OUT_OF_SCOPE.to_owned()),
            },
            StorageError::Unavailable(detail) => Self::BackendUnavailable(detail),
            // Detected at bootstrap and fatal there. A runtime occurrence is a
            // contract violation of the plugin, hence internal.
            StorageError::SchemaVersionMismatch {
                installed,
                expected,
            } => Self::Internal(format!(
                "storage reported schema major {installed} (expected {expected}) after bootstrap"
            )),
            StorageError::Internal(detail) => Self::Internal(detail),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_tests.rs"]
mod error_tests;
