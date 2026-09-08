//! [`PluginError`] — the plugin's own evaluation-failure type.
//!
//! Every failure the evaluation pipeline can produce is one variant here, and
//! every variant carries its own observability classification through
//! [`PluginError::labels`]. The SDK's `AuthZResolverError` is produced once,
//! at the `evaluate()` boundary, by [`From<PluginError>`].
//!
//! # Why this exists
//!
//! `AuthZResolverError` has three variants — `ServiceUnavailable(String)`,
//! `Internal(String)` and `NoPluginAvailable` — and no `InvalidRequest`, so it
//! cannot express the client-fault/system-fault split that observability needs.
//! Carrying that split on a variant of this type instead keeps it off the
//! message text: `labels()` is an exhaustive `match` with no wildcard arm, so a
//! new failure mode cannot be added without the compiler demanding its
//! `error_type` and `fail_closed` labels. A reword cannot reclassify a deny
//! between `invalid_request` (no page) and a fail-closed system fault (which
//! pages on-call).

use toolkit_macros::domain_model;

use crate::domain::deny::{service_errors, validation_messages as vm};
use crate::domain::metrics_port::{ErrorType, FailClosedReason};

/// A failure raised inside the evaluation pipeline.
///
/// Grouped by classification rather than by producing module, because the
/// classification is what the variant exists to carry.
///
/// `Clone` — unlike `AuthZResolverError` — so [`crate::domain::hierarchy_cache`]
/// can hand the same fetch failure to every in-flight waiter without
/// reconstructing it variant-by-variant.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PluginError {
    // ---- Client faults: a malformed request. Not fail-closed: nothing is
    // ---- degraded, the caller asked for something incoherent. -------------
    /// `subject.subject_type` is present but not a recognized principal type.
    UnknownSubjectType {
        /// The rejected value, echoed into the message.
        value: String,
    },
    /// `action.name` is empty.
    InvalidOperationEmpty,
    /// `action.name` contains a wildcard (`*` or `?`).
    InvalidOperationWildcard,
    /// `resource.resource_type` is empty.
    MissingResourceType,
    /// `subject.properties["tenant_id"]` is present but is not a string, or is
    /// a string that does not parse as a UUID.
    ///
    /// A present-but-unreadable claim is a malformed request, not an outage:
    /// classifying it as a system fault paged on-call for a phantom outage.
    UnreadableSubjectTenant {
        /// Why the claim could not be read, appended to the message.
        detail: String,
    },

    // ---- Transient dependency outages. Fail-closed and retryable. --------
    /// The RBAC service call failed.
    RbacUnavailable,
    /// The tenant resolver call failed for a reason other than
    /// not-found/unauthorized.
    TenantResolverUnavailable,
    /// The resource-group resolver call failed for a reason other than
    /// not-found/permission-denied.
    ResourceGroupUnavailable,
    /// The GTS types registry could not be reached during Strict-mode
    /// validation.
    ///
    /// Its own variant rather than a resolver one: bucketing a registry outage
    /// under `resolver_timeout` pages on-call for the wrong dependency.
    GtsRegistryUnavailable,

    // ---- Deterministic scope-resolution failures. Fail-closed but NOT
    // ---- transient: retrying cannot help. --------------------------------
    /// A granted scope named a tenant the resolver would not resolve — deleted,
    /// or not accessible. The tenant-resolver SDK overloads `TenantNotFound`
    /// to mean either, and neither self-heals.
    TenantNotFound,
    /// A granted group scope named a resource group the resolver would not
    /// resolve. Resource-group analogue of [`Self::TenantNotFound`].
    ResourceGroupNotFound,

    // ---- Producer contract drift. -----------------------------------------
    /// RBAC returned an allow whose aggregate scope could not be derived from
    /// its contributing role assignments — partial payload corruption or
    /// producer contract drift, not an outage.
    RbacScopeProvenanceInvalid,

    // ---- Everything else. -------------------------------------------------
    /// An unclassified internal failure. Fail-closed as
    /// `all_constraints_failed`.
    ///
    /// Reach for a new variant instead whenever the failure has a
    /// classification of its own — this one exists for genuinely unexpected
    /// states (an upstream returning a shape its contract forbids, a decode
    /// that cannot fail), and it is the arm that pages on-call.
    Internal {
        /// Human-readable detail; carried through to the SDK error message.
        detail: String,
    },
}

impl PluginError {
    pub(crate) fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }

    /// The `error_type` metric label and, when the failure is fail-closed, the
    /// `fail_closed` reason label (§3.13).
    ///
    /// Deliberately exhaustive with no `_` arm: adding a variant must be a
    /// compile error here, since an unclassified failure defaults to the arm
    /// that pages on-call.
    pub(crate) const fn labels(&self) -> (ErrorType, Option<FailClosedReason>) {
        match self {
            // Client faults — a 4xx-class fault degrades nothing, so no
            // fail-closed reason.
            Self::UnknownSubjectType { .. }
            | Self::InvalidOperationEmpty
            | Self::InvalidOperationWildcard
            | Self::MissingResourceType
            | Self::UnreadableSubjectTenant { .. } => (ErrorType::InvalidRequest, None),

            Self::RbacUnavailable => (
                ErrorType::RbacUnavailable,
                Some(FailClosedReason::RbacUnavailable),
            ),
            Self::TenantResolverUnavailable => (
                ErrorType::TenantResolverTimeout,
                Some(FailClosedReason::ResolverTimeout),
            ),
            Self::ResourceGroupUnavailable => (
                ErrorType::RgResolverTimeout,
                Some(FailClosedReason::ResolverTimeout),
            ),
            Self::GtsRegistryUnavailable => (
                ErrorType::GtsRegistryUnavailable,
                Some(FailClosedReason::GtsRegistryUnavailable),
            ),

            // Deterministic, so `scope_unresolvable` rather than a timeout
            // reason: a stale grant naming a missing tenant must not read as an
            // outage.
            Self::TenantNotFound => (
                ErrorType::TenantResolverNotFound,
                Some(FailClosedReason::ScopeUnresolvable),
            ),
            Self::ResourceGroupNotFound => (
                ErrorType::RgResolverNotFound,
                Some(FailClosedReason::ScopeUnresolvable),
            ),

            Self::RbacScopeProvenanceInvalid => (
                ErrorType::RbacScopeProvenanceInvalid,
                Some(FailClosedReason::RbacScopeProvenanceInvalid),
            ),

            Self::Internal { .. } => (
                ErrorType::Unexpected,
                Some(FailClosedReason::AllConstraintsFailed),
            ),
        }
    }
}

impl std::fmt::Display for PluginError {
    /// The message the SDK error carries. Text only — no consumer classifies
    /// on it; see [`PluginError::labels`].
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSubjectType { value } => {
                write!(f, "{} {value}", vm::UNKNOWN_SUBJECT_TYPE_PREFIX)
            }
            Self::InvalidOperationEmpty => f.write_str(vm::INVALID_OPERATION_EMPTY),
            Self::InvalidOperationWildcard => f.write_str(vm::INVALID_OPERATION_WILDCARD),
            Self::MissingResourceType => f.write_str(vm::MISSING_RESOURCE_TYPE),
            Self::UnreadableSubjectTenant { detail } => {
                write!(f, "{} {detail}", vm::UNREADABLE_SUBJECT_TENANT_PREFIX)
            }
            Self::RbacUnavailable => f.write_str(service_errors::RBAC_UNAVAILABLE),
            Self::TenantResolverUnavailable => {
                f.write_str(service_errors::TENANT_RESOLVER_UNAVAILABLE)
            }
            Self::ResourceGroupUnavailable => {
                f.write_str(service_errors::RESOURCE_GROUP_UNAVAILABLE)
            }
            Self::GtsRegistryUnavailable => f.write_str(service_errors::GTS_REGISTRY_UNAVAILABLE),
            Self::TenantNotFound => f.write_str(service_errors::TENANT_NOT_FOUND),
            Self::ResourceGroupNotFound => f.write_str(service_errors::RESOURCE_GROUP_NOT_FOUND),
            Self::RbacScopeProvenanceInvalid => {
                f.write_str(service_errors::RBAC_SCOPE_PROVENANCE_INVALID)
            }
            Self::Internal { detail } => f.write_str(detail),
        }
    }
}

impl std::error::Error for PluginError {}

// DE1302: the source error cannot be preserved here. Every
// `AuthZResolverError` variant carries a `String` and nothing else, so there is
// nowhere to put a `source()` — and the SDK type belongs to
// `authz-resolver-sdk`, not to this crate. The chain is not silently lost
// either: `PluginError::labels` has already recorded the classification on the
// metric before this projection runs, which is the part downstream consumers
// act on. Same allow, for the same reason, as `toolkit-canonical-errors` and
// `file-storage`'s `error_convert`.
//
// TODO: remove this allow if `AuthZResolverError` ever grows a variant that can
// hold a boxed source.
#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<PluginError> for authz_resolver_sdk::AuthZResolverError {
    /// Project onto the SDK's three variants. Lossy on purpose — the
    /// distinctions the SDK cannot express have already been recorded by
    /// [`PluginError::labels`] before this runs.
    ///
    /// `ServiceUnavailable` covers the transient dependency outages, matching
    /// what the SDK variant means to a caller deciding whether to retry. It
    /// also carries the deterministic, non-retryable failures — a scope naming
    /// a missing tenant, provenance drift — because the SDK has no variant that
    /// fits them and integration tests pin the exact message strings.
    fn from(err: PluginError) -> Self {
        use authz_resolver_sdk::AuthZResolverError as Sdk;
        let message = err.to_string();
        match err {
            PluginError::RbacUnavailable
            | PluginError::TenantResolverUnavailable
            | PluginError::ResourceGroupUnavailable
            | PluginError::GtsRegistryUnavailable
            | PluginError::TenantNotFound
            | PluginError::ResourceGroupNotFound => Sdk::ServiceUnavailable(message),

            PluginError::UnknownSubjectType { .. }
            | PluginError::InvalidOperationEmpty
            | PluginError::InvalidOperationWildcard
            | PluginError::MissingResourceType
            | PluginError::UnreadableSubjectTenant { .. }
            | PluginError::RbacScopeProvenanceInvalid
            | PluginError::Internal { .. } => Sdk::Internal(message),
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
