//! `From<DomainError> for CanonicalError`: the boundary lift of DESIGN
//! section 3.3, "Error Model". The HTTP status is a property of the canonical
//! category; fine-grained discriminators ride as reason tokens.
//!
//! Kept out of `domain/`: the lift names GTS resource types and, later, may
//! classify backend failures with `toolkit-db` helpers the domain layer must
//! not import.

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::{DomainError, ResourceKind};

#[resource_error(gts_id!("cf.qe.resource.quota.v1~"))]
pub(crate) struct QuotaResource;

#[resource_error(gts_id!("cf.qe.resource.policy.v1~"))]
pub(crate) struct PolicyResource;

#[resource_error(gts_id!("cf.qe.resource.lease.v1~"))]
pub(crate) struct LeaseResource;

#[resource_error(gts_id!("cf.qe.resource.operation.v1~"))]
pub(crate) struct OperationResource;

/// Closed reason tokens of the canonical envelope.
pub mod reason {
    /// PDP denial, compile failure, or the gateway's tenant gate.
    pub const AUTHZ: &str = "AUTHZ";
    /// Bootstrap has not completed.
    pub const NOT_READY: &str = "NOT_READY";
    /// A dependency is unreachable.
    pub const DEPENDENCY_UNAVAILABLE: &str = "DEPENDENCY_UNAVAILABLE";
}

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            // --- 400 InvalidArgument ---
            DomainError::InvalidArgument { field, reason } => QuotaResource::invalid_argument()
                .with_field_violation(field, format!("invalid argument {field}: {reason}"), reason)
                .create(),
            DomainError::ProjectionNotRegistered { projection } => {
                QuotaResource::invalid_argument()
                    .with_field_violation(
                        "projection_type",
                        format!("projection {projection} is not registered"),
                        "PROJECTION_NOT_REGISTERED",
                    )
                    .with_resource(projection)
                    .create()
            }

            // --- 400 FailedPrecondition ---
            DomainError::LeaseNotActive { token } => LeaseResource::failed_precondition()
                .with_precondition_violation(
                    token.to_string(),
                    format!("lease {token} is not active"),
                    "LEASE_NOT_ACTIVE",
                )
                .create(),
            DomainError::OverCommitNotAuthorized { reserved, actual } => {
                LeaseResource::failed_precondition()
                    .with_precondition_violation(
                        "actual_amount",
                        format!("commit amount {actual} exceeds reserved amount {reserved}"),
                        "OVER_COMMIT_NOT_AUTHORIZED",
                    )
                    .create()
            }
            DomainError::CapBelowConsumed { new_cap, consumed } => {
                QuotaResource::failed_precondition()
                    .with_precondition_violation(
                        "cap",
                        format!("cap {new_cap} is below the consumed amount {consumed}"),
                        "CAP_BELOW_CONSUMED",
                    )
                    .create()
            }
            DomainError::QuotaDeactivated { id } => QuotaResource::failed_precondition()
                .with_precondition_violation(
                    id.clone(),
                    format!("quota {id} is deactivated"),
                    "QUOTA_DEACTIVATED",
                )
                .create(),
            DomainError::PeriodClosed => QuotaResource::failed_precondition()
                .with_precondition_violation(
                    "period",
                    "the target period is closed",
                    "PERIOD_CLOSED",
                )
                .create(),
            DomainError::MetricNotRegistered { metric } => QuotaResource::failed_precondition()
                .with_precondition_violation(
                    metric.clone(),
                    format!("metric {metric} is not registered"),
                    "METRIC_NOT_REGISTERED",
                )
                .create(),
            DomainError::MetricNotQuotaGated { metric } => QuotaResource::failed_precondition()
                .with_precondition_violation(
                    metric.clone(),
                    format!("metric {metric} is not quota-gated"),
                    "METRIC_NOT_QUOTA_GATED",
                )
                .create(),
            DomainError::UnknownPolicyVersion { policy_id, version } => {
                PolicyResource::failed_precondition()
                    .with_precondition_violation(
                        format!("{policy_id}@{version}"),
                        format!("unknown version {version} of policy {policy_id}"),
                        "UNKNOWN_POLICY_VERSION",
                    )
                    .create()
            }
            DomainError::VersionRolledBack { policy_id, version } => {
                PolicyResource::failed_precondition()
                    .with_precondition_violation(
                        format!("{policy_id}@{version}"),
                        format!("version {version} of policy {policy_id} was rolled back"),
                        "VERSION_ROLLED_BACK",
                    )
                    .create()
            }

            // --- 403 PermissionDenied (no PDP detail on the wire) ---
            DomainError::PdpDenied { .. } => QuotaResource::permission_denied()
                .with_reason(reason::AUTHZ)
                .create(),

            // --- 404 NotFound ---
            DomainError::NotFound { kind, id } => {
                let detail = format!("{kind} {id} not found");
                match kind {
                    ResourceKind::Quota => {
                        QuotaResource::not_found(detail).with_resource(id).create()
                    }
                    ResourceKind::Policy => {
                        PolicyResource::not_found(detail).with_resource(id).create()
                    }
                    ResourceKind::Lease => {
                        LeaseResource::not_found(detail).with_resource(id).create()
                    }
                    ResourceKind::Operation => OperationResource::not_found(detail)
                        .with_resource(id)
                        .create(),
                }
            }

            // --- 409 Aborted (safe to retry) ---
            DomainError::IdempotencyPayloadMismatch => {
                OperationResource::aborted("idempotency key replayed with a different payload")
                    .with_reason("IDEMPOTENCY_PAYLOAD_MISMATCH")
                    .create()
            }
            DomainError::VersionConflict { expected, actual } => PolicyResource::aborted(format!(
                "version conflict: expected {expected}, found {actual}"
            ))
            .with_reason("VERSION_CONFLICT")
            .create(),
            DomainError::LeaseContentionTimeout => {
                LeaseResource::aborted("acquisition contention timeout elapsed")
                    .with_reason("LEASE_CONTENTION_TIMEOUT")
                    .create()
            }

            // --- 429 ResourceExhausted ---
            DomainError::LeaseInflightLimitExceeded => LeaseResource::resource_exhausted(
                "active-lease cap reached for the (tenant, metric) pair",
            )
            .with_quota_violation("(tenant, metric)", "LEASE_INFLIGHT_LIMIT_EXCEEDED")
            .create(),

            // --- 503 ServiceUnavailable ---
            DomainError::NotReady { dependency } => CanonicalError::service_unavailable()
                .with_detail(format!(
                    "{}: quota enforcement is not ready ({dependency})",
                    reason::NOT_READY
                ))
                .create(),
            DomainError::PdpUnavailable(_) => CanonicalError::service_unavailable()
                .with_detail(format!(
                    "{}: authorization service unavailable",
                    reason::DEPENDENCY_UNAVAILABLE
                ))
                .create(),
            DomainError::BackendUnavailable(_) => CanonicalError::service_unavailable()
                .with_detail(format!(
                    "{}: storage backend unavailable",
                    reason::DEPENDENCY_UNAVAILABLE
                ))
                .create(),
            DomainError::TypesRegistryUnavailable(_) => CanonicalError::service_unavailable()
                .with_detail(format!(
                    "{}: types registry unavailable",
                    reason::DEPENDENCY_UNAVAILABLE
                ))
                .create(),
            DomainError::PluginNotFound { kind, .. }
            | DomainError::PluginClientNotRegistered { kind, .. }
            | DomainError::InvalidPluginInstance { kind, .. } => {
                CanonicalError::service_unavailable()
                    .with_detail(format!(
                        "{}: {kind} plugin unavailable",
                        reason::DEPENDENCY_UNAVAILABLE
                    ))
                    .create()
            }
            DomainError::ClusterUnavailable(_) => CanonicalError::service_unavailable()
                .with_detail(format!(
                    "{}: cluster unavailable",
                    reason::DEPENDENCY_UNAVAILABLE
                ))
                .create(),

            // --- 500 Internal (opaque; never carries internal detail) ---
            DomainError::SchemaVersionMismatch { .. } | DomainError::Internal(_) => {
                CanonicalError::internal("internal error").create()
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "canonical_mapping_tests.rs"]
mod canonical_mapping_tests;
