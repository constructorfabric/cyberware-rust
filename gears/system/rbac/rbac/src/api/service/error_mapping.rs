//! Boundary mapping from internal [`DomainError`] to the public
//! [`RbacServiceError`] wire contract.
//!
//! `DomainError` is the single internal currency every domain function,
//! port, and repository returns. The SDK enum is the stable
//! `#[non_exhaustive]` contract consumed by other modules via
//! `ClientHub`. Keeping the lift in one place means adding an internal
//! variant is a non-breaking change unless we also widen the SDK enum.

use rbac_sdk::error::RbacServiceError;

use crate::domain::error::{BoxError, DomainError};

/// Severity for [`log_dropped_cause`].
#[derive(Clone, Copy)]
enum Level {
    /// Dependency outage — retryable, so `warn`.
    Warn,
    /// Internal failure — `error`.
    Error,
}

/// Log an error chain that stops at the SDK boundary.
///
/// `RbacServiceError` has no field for a cause, so only the curated
/// `detail` crosses. Without this the root cause of every 500/503 was
/// lost — which is what left `db error: sea-orm (text redacted)` as the
/// entire diagnostic for a failed query, even though `From<DbError>`
/// deliberately keeps the original in an `Arc`.
fn log_dropped_cause(level: Level, cause: Option<&BoxError>, detail: &str) {
    let Some(cause) = cause else {
        return;
    };
    match level {
        Level::Warn => tracing::warn!(
            target: "rbac.internal",
            %cause,
            %detail,
            "dependency unavailable; cause is dropped at the SDK boundary"
        ),
        Level::Error => tracing::error!(
            target: "rbac.internal",
            %cause,
            %detail,
            "internal error; cause is dropped at the SDK boundary"
        ),
    }
}

/// Variant-by-variant lift from [`DomainError`] to the SDK enum.
///
/// Match arms are grouped by the AIP-193 category (`InvalidArgument`,
/// `NotFound`, `AlreadyExists`, `Aborted`, `FailedPrecondition`,
/// `PermissionDenied`, `ServiceUnavailable`, `Unimplemented`,
/// `Internal`) — the same grouping `DomainError` itself uses, so adding
/// a variant lands here in the right place by inspection.
///
/// `clippy::match_same_arms` is suppressed: multiple internal
/// categories collapse to the same SDK variant (the wire contract has
/// fewer buckets than the internal vocabulary on purpose), but the
/// arms are kept distinct so adding a variant lands next to the
/// AIP-193 category it belongs to.
#[allow(clippy::match_same_arms)]
impl From<DomainError> for RbacServiceError {
    fn from(err: DomainError) -> Self {
        match err {
            // ---- InvalidArgument (HTTP 400) ----
            DomainError::Validation { detail } => Self::Validation { message: detail },
            DomainError::ImmutableFieldRejected { field } => Self::immutable_field_rejected(field),
            DomainError::InvalidPermissionRule { detail } => Self::InvalidPermissionRule { detail },
            DomainError::InvalidScopeFormat { scope } => Self::InvalidScopeFormat { scope },

            // ---- NotFound (HTTP 404) ----
            DomainError::RoleDefinitionNotFound { id } => Self::RoleDefinitionNotFound { id },
            DomainError::RoleAssignmentNotFound { id } => Self::RoleAssignmentNotFound { id },
            DomainError::ScopeNotFound { scope } => Self::ScopeNotFound { scope },
            DomainError::GroupPrincipalNotFound { principal_id } => {
                Self::GroupPrincipalNotFound { principal_id }
            }

            // ---- AlreadyExists (HTTP 409) ----
            DomainError::RoleDefinitionNameTaken {
                name,
                owner_tenant_id,
            } => Self::RoleDefinitionNameTaken {
                name,
                owner_tenant_id,
            },
            DomainError::RoleDefinitionNameReservedByBuiltin { name } => {
                Self::RoleDefinitionNameReservedByBuiltin { name }
            }
            DomainError::RoleAssignmentDuplicate {
                role_definition_id,
                principal_type,
                principal_id,
                scope,
            } => Self::RoleAssignmentDuplicate {
                role_definition_id,
                principal_type,
                principal_id,
                scope,
            },
            DomainError::AlreadyExists { detail } => Self::Conflict { message: detail },
            DomainError::BuiltInRoleNotModifiable { role_definition_id } => {
                Self::BuiltInRoleNotModifiable { role_definition_id }
            }

            // ---- Aborted (HTTP 409) ----
            //
            // Serialization-conflict surfaces as a generic conflict in
            // the SDK enum (no dedicated variant). `StaleEtag` is the
            // 412 path.
            DomainError::Aborted { reason: _, detail } => Self::Conflict { message: detail },
            DomainError::StaleEtag { current_etag } => {
                Self::OptimisticConcurrencyStale { current_etag }
            }

            // ---- FailedPrecondition (HTTP 409 / 400) ----
            DomainError::RoleDefinitionAssignmentsExist { role_definition_id } => {
                Self::RoleDefinitionAssignmentsExist { role_definition_id }
            }
            // The handler is responsible for upgrading "missing FK
            // referent" into a more specific not-found if it knows the
            // caller's intent (e.g. POST /role-assignments → 404 on the
            // role definition). The default surface is a 409 conflict.
            DomainError::RoleDefinitionMissing { role_definition_id } => Self::Conflict {
                message: format!("referenced role definition {role_definition_id} does not exist"),
            },
            DomainError::Conflict { detail } => Self::Conflict { message: detail },
            DomainError::ScopeNotWithinAssignableScopes {
                scope,
                assignable_scopes,
            } => Self::ScopeNotWithinAssignableScopes {
                scope,
                assignable_scopes,
            },
            DomainError::GroupPrincipalRootScopeForbidden => Self::GroupPrincipalRootScopeForbidden,
            DomainError::OptimisticConcurrencyMissing => Self::OptimisticConcurrencyMissing,
            DomainError::OwnerTenantRequired => Self::OwnerTenantRequired,
            DomainError::OwnerTenantMismatch => Self::OwnerTenantMismatch,

            // ---- PermissionDenied (HTTP 403) ----
            DomainError::AuthorizationDenied { detail, cause } => {
                // The SDK `AuthorizationDenied` variant carries only a
                // `message: String` — the typed `cause` chain is lost
                // at this boundary. Log it before dropping so operators
                // still have the upstream diagnostic for audit
                // correlation. Matches how `If-Match` parse errors are
                // handled.
                if let Some(cause) = cause.as_ref() {
                    tracing::warn!(
                        target: "rbac.authz",
                        detail = %detail,
                        cause = %cause,
                        "AuthorizationDenied at the SDK boundary; cause not forwarded to the wire"
                    );
                }
                Self::AuthorizationDenied { message: detail }
            }

            // ---- ServiceUnavailable (HTTP 503) ----
            //
            // A transient outage keeps its own typed variant, so it stays
            // a retryable 503 with its retry hint. Collapsing it into
            // `Internal` would turn every tenant-resolver blip into a 500
            // telling the caller not to retry.
            //
            // `cause` is dropped here on purpose: it is an operator-side
            // error chain and the SDK variant has no field for it, so
            // only the curated `detail` crosses the boundary. The REST
            // layer logs that and returns a generic envelope.
            DomainError::ServiceUnavailable {
                detail,
                retry_after,
                cause,
            } => {
                log_dropped_cause(Level::Warn, cause.as_ref(), &detail);
                Self::ServiceUnavailable {
                    detail,
                    retry_after_seconds: retry_after.map(|d| d.as_secs()),
                }
            }
            DomainError::DependencyUnavailable { dependency } => Self::DependencyUnavailable {
                dependency: dependency.to_owned(),
            },

            // ---- Unimplemented (HTTP 501) ----
            DomainError::UnsupportedOperation { detail } => Self::Internal { message: detail },

            // ---- Internal (HTTP 500) ----
            DomainError::Internal { diagnostic, cause } => {
                log_dropped_cause(Level::Error, cause.as_ref(), &diagnostic);
                Self::Internal {
                    message: diagnostic,
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "error_mapping_tests.rs"]
mod error_mapping_tests;
