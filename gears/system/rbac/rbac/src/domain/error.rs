//! RBAC domain error type.
//!
//! Internal-only — never crosses module boundaries. On every boundary
//! (REST handlers, inter-module SDK callers via `ClientHub`) this type
//! is converted to either
//! [`rbac_sdk::error::RbacServiceError`] (SDK boundary) or
//! [`toolkit_canonical_errors::CanonicalError`] (REST boundary), following
//! the AIP-193 error model. Public HTTP status codes and the stable
//! error-code taxonomy are defined by the canonical-errors contract;
//! RBAC's role is to map domain failures onto AIP-193 categories, not to
//! invent its own HTTP-status table.
//!
//! # Layering
//!
//! `DomainError` is pure — no `sea_orm::DbErr`, no `toolkit_db` types,
//! no `crate::infra` imports. The DB-aware classification ladder
//! (SQLSTATE 40001 / 23505 / availability / unclassified) lives in
//! [`crate::infra::canonical_mapping`] together with the `From` impls
//! that produce `DomainError` from raw DB errors. The mapping that
//! lifts `DomainError` into the public SDK / REST envelopes lives in
//! `api::service::error_mapping` and [`crate::api::rest::error`]
//! respectively.

use std::sync::Arc;
use std::time::Duration;

use rbac_sdk::models::PrincipalType;
use thiserror::Error;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// Shared, cloneable error pointer used for the `cause` chain on variants that
/// carry an upstream error (`AuthorizationDenied`, `ServiceUnavailable`,
/// `Internal`).
///
/// `Arc<dyn Error>` rather than `Box<dyn Error>`: `Box` is not `Clone`, so the
/// manual `Clone` impl could not preserve the source, and every clone — test
/// stub, retry classification, logging — would erase the diagnostic chain. The
/// wrapped error still surfaces through [`std::error::Error::source`] via the
/// trailing `&**arc` deref. Named `BoxError` to match the crate's convention.
pub(crate) type BoxError = Arc<dyn std::error::Error + Send + Sync + 'static>;

/// RBAC domain-internal error.
///
/// Variants are grouped by the AIP-193 category they map to at the
/// boundary; the grouping is preserved in declaration order so reviewers
/// can eyeball-check exhaustiveness against the
/// `From<DomainError> for RbacServiceError` impl in
/// `api::service::error_mapping`.
#[domain_model]
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DomainError {
    // ---- InvalidArgument (HTTP 400) ----
    /// Generic structural / domain validation failure (malformed
    /// payload, invalid principal type, etc.). Maps to
    /// [`rbac_sdk::error::RbacServiceError::Validation`].
    #[error("validation failed: {detail}")]
    Validation { detail: String },

    /// PATCH body referenced an immutable field
    /// (`id` / `is_built_in` / `owner_tenant_id` / `created_at` /
    /// `created_by`). Detected at the REST boundary because it
    /// requires the raw DTO shape; the service surfaces this
    /// variant only after authz so an unauthorized caller cannot
    /// distinguish a body-shape rejection from the missing-row
    /// 404. Maps to
    /// [`rbac_sdk::error::RbacServiceError::ImmutableFieldRejected`].
    #[error("immutable field rejected: {field}")]
    ImmutableFieldRejected { field: String },

    /// A permission rule in the request body is malformed (unknown
    /// `operation`/`target_type` shape, illegal characters, etc.).
    /// Carries a `detail` string identifying the offending rule.
    #[error("invalid permission rule: {detail}")]
    InvalidPermissionRule { detail: String },

    /// Scope path did not parse against the documented forms
    /// (`/`, `/tenants/{uuid}`,
    /// `/tenants/{uuid}/resourceGroups/{uuid}`).
    #[error("invalid scope format: {scope}")]
    InvalidScopeFormat { scope: String },

    // ---- NotFound (HTTP 404) ----
    /// Role definition lookup miss.
    #[error("role definition not found: {id}")]
    RoleDefinitionNotFound { id: Uuid },

    /// Role assignment lookup miss.
    #[error("role assignment not found: {id}")]
    RoleAssignmentNotFound { id: Uuid },

    /// Scope path does not exist in the tenant hierarchy or the
    /// resource-group module.
    #[error("scope not found: {scope}")]
    ScopeNotFound { scope: String },

    /// Group principal lookup miss in the resource-group module.
    #[error("group principal not found: {principal_id}")]
    GroupPrincipalNotFound { principal_id: Uuid },

    // ---- AlreadyExists (HTTP 409) ----
    /// `(name, owner_tenant_id)` uniqueness was violated for a custom
    /// role definition.
    #[error("role definition name already in use: {name}")]
    RoleDefinitionNameTaken {
        name: String,
        owner_tenant_id: Option<Uuid>,
    },

    /// Case-insensitive collision against a built-in role name.
    #[error("role definition name '{name}' is reserved by a built-in role")]
    RoleDefinitionNameReservedByBuiltin { name: String },

    /// `uq_assignment` was violated — the
    /// `(role_definition_id, principal_type, principal_id, scope)` tuple
    /// already exists.
    #[error("duplicate role assignment: role={role_definition_id}, scope={scope}")]
    RoleAssignmentDuplicate {
        role_definition_id: Uuid,
        /// Typed enum, mirrors the SDK shape one crate over.
        principal_type: PrincipalType,
        principal_id: String,
        scope: String,
    },

    /// Generic uniqueness violation from the storage layer that the
    /// classifier could not attribute to a specific RBAC constraint.
    /// Produced by [`crate::infra::canonical_mapping::classify_db_err_to_domain`]
    /// when the SQLSTATE is `23505` but no constraint-name match exists.
    #[error("already exists: {detail}")]
    AlreadyExists { detail: String },

    /// PATCH/DELETE was attempted against a built-in role definition.
    /// Built-ins are immutable. Maps to
    /// [`rbac_sdk::error::RbacServiceError::BuiltInRoleNotModifiable`].
    #[error("role definition {role_definition_id} is built-in and cannot be modified")]
    BuiltInRoleNotModifiable { role_definition_id: Uuid },

    // ---- Aborted (HTTP 409) ----
    /// Retry-budget-exhausted serialization failure (SQLSTATE `40001`
    /// or the `SQLite` analogue, after every retry attempt). `reason` is
    /// the canonical machine-readable token (e.g.
    /// `"SERIALIZATION_CONFLICT"`).
    #[error("aborted: {detail}")]
    Aborted { reason: String, detail: String },

    /// Compare-and-swap on `updated_at` did not match — another writer
    /// mutated the row between the read and the write. Carries the
    /// row's current `ETag` so the handler can surface it on the 412
    /// response (empty when the row vanished after the SELECT).
    #[error("optimistic concurrency conflict: row was modified")]
    StaleEtag { current_etag: String },

    // ---- FailedPrecondition (HTTP 409) ----
    /// A delete was attempted against a role definition that still has
    /// active role assignments.
    #[error("role definition {role_definition_id} still has active assignments")]
    RoleDefinitionAssignmentsExist { role_definition_id: Uuid },

    /// FK violation on `role_definition_id` — the referenced role no
    /// longer exists. Handlers may upgrade this to
    /// `RoleDefinitionNotFound` at the SDK boundary.
    #[error("referenced role definition {role_definition_id} does not exist")]
    RoleDefinitionMissing { role_definition_id: Uuid },

    /// Generic precondition failure not covered by a more specific
    /// variant.
    #[error("precondition failed: {detail}")]
    Conflict { detail: String },

    /// Role-assignment `scope` is not admitted by the role's
    /// `assignable_scopes` (the descendant rule). Maps to HTTP 400 via
    /// [`rbac_sdk::error::RbacServiceError::ScopeNotWithinAssignableScopes`].
    #[error("scope '{scope}' is not within the role's assignable_scopes {assignable_scopes:?}")]
    ScopeNotWithinAssignableScopes {
        scope: String,
        assignable_scopes: Vec<String>,
    },

    /// `principal_type = Group` assignment targeted root scope `/`.
    /// Maps to HTTP 400.
    #[error("group principals MUST NOT be assigned at root scope `/`")]
    GroupPrincipalRootScopeForbidden,

    /// PATCH/DELETE without an `If-Match` header. Maps to HTTP 428
    /// Precondition Required.
    #[error("optimistic concurrency requires an `If-Match` precondition")]
    OptimisticConcurrencyMissing,

    /// Root-scoped caller omitted `owner_tenant_id` on a write. Maps to
    /// HTTP 400.
    #[error("owner_tenant_id is required for root-scoped callers")]
    OwnerTenantRequired,

    /// Tenant-scoped caller supplied an `owner_tenant_id` that does
    /// not match their authentication context. Maps to HTTP 403.
    #[error("owner tenant mismatch: tenant-scoped callers cannot manage roles in another tenant")]
    OwnerTenantMismatch,

    // ---- PermissionDenied (HTTP 403) ----
    /// Authorization rejection. `cause` is `Some` only when the denial
    /// originates from an upstream PEP/PDP transport; local rejections
    /// leave it `None`.
    #[error("authorization denied: {detail}")]
    AuthorizationDenied {
        detail: String,
        #[source]
        cause: Option<BoxError>,
    },

    // ---- ServiceUnavailable (HTTP 503) ----
    /// Transient infrastructure outage (pool acquire timeout,
    /// connection dropped, IO error). `retry_after` populates the
    /// canonical envelope's `retry_after_seconds` when the producer has
    /// a defensible hint; `cause` carries the upstream error chain for
    /// non-DB sources.
    #[error("service unavailable: {detail}")]
    ServiceUnavailable {
        detail: String,
        retry_after: Option<Duration>,
        #[source]
        cause: Option<BoxError>,
    },

    /// A required upstream dependency resolved through `ClientHub` is
    /// absent or unhealthy (`TypesRegistryClient`,
    /// `TenantResolverClient`, etc.). `dependency` is a static label
    /// surfaced verbatim on the SDK envelope.
    #[error("dependency unavailable: {dependency}")]
    DependencyUnavailable { dependency: &'static str },

    // ---- Unimplemented (HTTP 501) ----
    /// The requested operation is not supported in the current
    /// deployment profile.
    #[error("operation not supported: {detail}")]
    UnsupportedOperation { detail: String },

    // ---- Internal (HTTP 500) ----
    /// Unclassified internal failure. `diagnostic` is recorded in the
    /// audit trail but MUST NOT be leaked through any public envelope
    /// verbatim; the boundary mapping forwards only a redacted string.
    /// `cause` carries the upstream error chain when available.
    #[error("internal error")]
    Internal {
        diagnostic: String,
        #[source]
        cause: Option<BoxError>,
    },
}

/// Manual `Clone` impl: `BoxError` is now `Arc<dyn Error>` so the
/// `cause` chain is preserved across clones (test stubs returning the
/// same canned error from multiple invocations, retry classification,
/// and audit logging all keep their `source()` chain intact). The
/// `#[domain_model]` macro defaults to `#[derive(Clone)]` which can't
/// clone trait-object fields, so the impl below stays manual.
impl Clone for DomainError {
    fn clone(&self) -> Self {
        match self {
            Self::Validation { detail } => Self::Validation {
                detail: detail.clone(),
            },
            Self::ImmutableFieldRejected { field } => Self::ImmutableFieldRejected {
                field: field.clone(),
            },
            Self::InvalidPermissionRule { detail } => Self::InvalidPermissionRule {
                detail: detail.clone(),
            },
            Self::InvalidScopeFormat { scope } => Self::InvalidScopeFormat {
                scope: scope.clone(),
            },
            Self::RoleDefinitionNotFound { id } => Self::RoleDefinitionNotFound { id: *id },
            Self::RoleAssignmentNotFound { id } => Self::RoleAssignmentNotFound { id: *id },
            Self::ScopeNotFound { scope } => Self::ScopeNotFound {
                scope: scope.clone(),
            },
            Self::GroupPrincipalNotFound { principal_id } => Self::GroupPrincipalNotFound {
                principal_id: *principal_id,
            },
            Self::RoleDefinitionNameTaken {
                name,
                owner_tenant_id,
            } => Self::RoleDefinitionNameTaken {
                name: name.clone(),
                owner_tenant_id: *owner_tenant_id,
            },
            Self::RoleDefinitionNameReservedByBuiltin { name } => {
                Self::RoleDefinitionNameReservedByBuiltin { name: name.clone() }
            }
            Self::RoleAssignmentDuplicate {
                role_definition_id,
                principal_type,
                principal_id,
                scope,
            } => Self::RoleAssignmentDuplicate {
                role_definition_id: *role_definition_id,
                principal_type: *principal_type,
                principal_id: principal_id.clone(),
                scope: scope.clone(),
            },
            Self::AlreadyExists { detail } => Self::AlreadyExists {
                detail: detail.clone(),
            },
            Self::BuiltInRoleNotModifiable { role_definition_id } => {
                Self::BuiltInRoleNotModifiable {
                    role_definition_id: *role_definition_id,
                }
            }
            Self::Aborted { reason, detail } => Self::Aborted {
                reason: reason.clone(),
                detail: detail.clone(),
            },
            Self::StaleEtag { current_etag } => Self::StaleEtag {
                current_etag: current_etag.clone(),
            },
            Self::RoleDefinitionAssignmentsExist { role_definition_id } => {
                Self::RoleDefinitionAssignmentsExist {
                    role_definition_id: *role_definition_id,
                }
            }
            Self::RoleDefinitionMissing { role_definition_id } => Self::RoleDefinitionMissing {
                role_definition_id: *role_definition_id,
            },
            Self::Conflict { detail } => Self::Conflict {
                detail: detail.clone(),
            },
            Self::ScopeNotWithinAssignableScopes {
                scope,
                assignable_scopes,
            } => Self::ScopeNotWithinAssignableScopes {
                scope: scope.clone(),
                assignable_scopes: assignable_scopes.clone(),
            },
            Self::GroupPrincipalRootScopeForbidden => Self::GroupPrincipalRootScopeForbidden,
            Self::OptimisticConcurrencyMissing => Self::OptimisticConcurrencyMissing,
            Self::OwnerTenantRequired => Self::OwnerTenantRequired,
            Self::OwnerTenantMismatch => Self::OwnerTenantMismatch,
            // `cause` is `Arc<dyn Error>` now — cloning shares the
            // pointer so `source()` chains survive across clones.
            Self::AuthorizationDenied { detail, cause } => Self::AuthorizationDenied {
                detail: detail.clone(),
                cause: cause.clone(),
            },
            Self::ServiceUnavailable {
                detail,
                retry_after,
                cause,
            } => Self::ServiceUnavailable {
                detail: detail.clone(),
                retry_after: *retry_after,
                cause: cause.clone(),
            },
            Self::DependencyUnavailable { dependency } => {
                Self::DependencyUnavailable { dependency }
            }
            Self::UnsupportedOperation { detail } => Self::UnsupportedOperation {
                detail: detail.clone(),
            },
            Self::Internal { diagnostic, cause } => Self::Internal {
                diagnostic: diagnostic.clone(),
                cause: cause.clone(),
            },
        }
    }
}

impl DomainError {
    /// Convenience constructor for [`Self::ServiceUnavailable`] without
    /// a retry-after hint or upstream cause.
    #[must_use]
    pub fn service_unavailable(detail: impl Into<String>) -> Self {
        Self::ServiceUnavailable {
            detail: detail.into(),
            retry_after: None,
            cause: None,
        }
    }

    /// Convenience constructor for [`Self::Internal`] without an
    /// upstream cause.
    #[must_use]
    pub fn internal(diagnostic: impl Into<String>) -> Self {
        Self::Internal {
            diagnostic: diagnostic.into(),
            cause: None,
        }
    }

    /// Convenience constructor for [`Self::AuthorizationDenied`]
    /// without an upstream cause.
    #[must_use]
    pub fn authorization_denied(detail: impl Into<String>) -> Self {
        Self::AuthorizationDenied {
            detail: detail.into(),
            cause: None,
        }
    }

    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::Validation {
            detail: detail.into(),
        }
    }

    /// Convenience constructor for [`Self::InvalidPermissionRule`].
    #[must_use]
    pub fn invalid_permission_rule(detail: impl Into<String>) -> Self {
        Self::InvalidPermissionRule {
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
