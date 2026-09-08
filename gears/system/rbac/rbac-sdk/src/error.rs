//! `RbacServiceError`: domain failure surface for the in-process trait.
//!
//! [`RbacServiceError`] is `#[non_exhaustive]`: external consumers MUST
//! include a wildcard (`_ =>`) arm when matching. The constructor methods
//! provide a stable build path that survives variant additions.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::role_assignment::PrincipalType;

/// Per-field validation failure surfaced inside [`RbacServiceError::ValidationFailed`].
///
/// `field` is a path-like identifier (e.g. `permissions[0].operation`).
/// `code` is a `snake_case` machine identifier consumers can branch on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    /// Path-like identifier of the offending field.
    pub field: String,
    /// Human-readable description of the failure.
    pub message: String,
    /// `snake_case` machine identifier.
    pub code: String,
}

impl FieldError {
    /// Construct a `FieldError` from its three components.
    #[must_use]
    pub fn new(
        field: impl Into<String>,
        message: impl Into<String>,
        code: impl Into<String>,
    ) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
            code: code.into(),
        }
    }
}

/// Canonical wire-form field names referenced by single-field validation
/// errors in the REST surface.
///
/// SDK consumers branching on a `FieldViolation`'s `field` slot should
/// match against `FieldViolationField::as_str()` rather than literals so
/// a rename caught in one place propagates everywhere.
///
/// `#[non_exhaustive]` — adding a new field name is NOT a breaking
/// change for downstream consumers. External pattern matches on this
/// enum MUST end with a wildcard `_ =>` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FieldViolationField {
    Permissions,
    Scope,
    OwnerTenantId,
    Limit,
    Cursor,
    PrincipalType,
}

impl FieldViolationField {
    /// Canonical `snake_case` wire-form name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Permissions => "permissions",
            Self::Scope => "scope",
            Self::OwnerTenantId => "owner_tenant_id",
            Self::Limit => "limit",
            Self::Cursor => "cursor",
            Self::PrincipalType => "principal_type",
        }
    }
}

/// Canonical `snake_case` machine identifiers for single-field validation
/// failures. The wire form of [`FieldError::code`] for these failures is
/// always the value returned by [`Self::as_str`].
///
/// `#[non_exhaustive]` — adding a new reason code is NOT a breaking
/// change for downstream consumers. External pattern matches on this
/// enum MUST end with a wildcard `_ =>` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FieldViolationReason {
    InvalidPermissionRule,
    ImmutableFieldRejected,
    OwnerTenantRequired,
    ScopeNotWithinAssignableScopes,
    GroupPrincipalRootScopeForbidden,
    GroupPrincipalTenantMismatch,
    InvalidScopeFormat,
    InvalidLimit,
    InvalidCursor,
    InvalidPrincipalType,
}

impl FieldViolationReason {
    /// Canonical `snake_case` wire-form code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidPermissionRule => "invalid_permission_rule",
            Self::ImmutableFieldRejected => "immutable_field_rejected",
            Self::OwnerTenantRequired => "owner_tenant_required",
            Self::ScopeNotWithinAssignableScopes => "scope_not_within_assignable_scopes",
            Self::GroupPrincipalRootScopeForbidden => "group_principal_root_scope_forbidden",
            Self::GroupPrincipalTenantMismatch => "group_principal_tenant_mismatch",
            Self::InvalidScopeFormat => "invalid_scope_format",
            Self::InvalidLimit => "invalid_limit",
            Self::InvalidCursor => "invalid_cursor",
            Self::InvalidPrincipalType => "invalid_principal_type",
        }
    }
}

/// Upper bound on the per-error `errors[]` array surfaced by
/// [`RbacServiceError::ValidationFailed`].
/// Caps the response envelope so a maliciously shaped request cannot amplify it.
pub const MAX_FIELD_ERRORS: usize = 100;

/// `code` used on the sentinel [`FieldError`] appended when the per-error
/// list is truncated by [`MAX_FIELD_ERRORS`].
pub const TRUNCATION_SENTINEL_CODE: &str = "too_many_errors";

/// Cap `errors` at [`MAX_FIELD_ERRORS`] entries; on overflow keep the first
/// `MAX_FIELD_ERRORS - 1` and append a sentinel `FieldError` whose
/// `code = TRUNCATION_SENTINEL_CODE` naming the suppressed count.
fn cap_field_errors(mut errors: Vec<FieldError>) -> Vec<FieldError> {
    if errors.len() <= MAX_FIELD_ERRORS {
        return errors;
    }
    let suppressed = errors.len() - (MAX_FIELD_ERRORS - 1);
    errors.truncate(MAX_FIELD_ERRORS - 1);
    errors.push(FieldError::new(
        "$truncated",
        format!("{suppressed} additional field error(s) suppressed"),
        TRUNCATION_SENTINEL_CODE,
    ));
    errors
}

/// Categorised error type returned by every method of `RbacServiceClientV1`.
///
/// Implementations MUST translate transport / database / dependency failures
/// into the appropriate variant rather than returning stringly-typed errors.
///
/// `#[non_exhaustive]` — match arms outside this crate MUST end with a
/// wildcard `_ =>` arm.
#[derive(Debug, Clone, Error)]
#[non_exhaustive]
pub enum RbacServiceError {
    /// The requested role definition does not exist.
    #[error("Role definition not found: {id}")]
    RoleDefinitionNotFound {
        /// Role definition ID that was looked up.
        id: Uuid,
    },

    /// The requested role assignment does not exist.
    #[error("Role assignment not found: {id}")]
    RoleAssignmentNotFound {
        /// Role assignment ID that was looked up.
        id: Uuid,
    },

    /// The request payload failed structural / domain validation
    /// (malformed request, invalid scope shape, invalid principal type, etc.).
    #[error("Validation error: {message}")]
    Validation {
        /// Human-readable message describing the validation failure.
        message: String,
    },

    /// The caller is authenticated but is not authorised to perform the
    /// requested action. Distinct from `Validation` so callers can branch on
    /// authz failures without parsing message strings.
    #[error("Authorization denied: {message}")]
    AuthorizationDenied {
        /// Human-readable message describing why the caller was denied.
        message: String,
    },

    /// A uniqueness or referential-integrity constraint was violated
    /// (duplicate role name within a tenant, duplicate role assignment,
    /// referenced role still has assignments, etc.).
    #[error("Conflict: {message}")]
    Conflict {
        /// Human-readable description of the conflict.
        message: String,
    },

    /// A required upstream dependency resolved through `ClientHub` is absent
    /// or unhealthy. Surfaces failures observed during `init()` dependency wiring.
    #[error("Dependency unavailable: {dependency}")]
    DependencyUnavailable {
        /// Identifier of the missing dependency (e.g. `TenantResolverClient`).
        dependency: String,
    },

    /// A transient infrastructure outage behind a named-dependency-free
    /// surface: a pool acquire timeout, a dropped connection, an upstream
    /// call that failed for a reason other than "not found".
    ///
    /// Distinct from [`Self::DependencyUnavailable`], which reports a
    /// dependency that is absent or unhealthy as a whole. Both map to 503;
    /// this one carries an operator diagnostic and an optional retry hint
    /// instead of a dependency tag. Without it, a transient outage has no
    /// typed home and collapses into [`Self::Internal`] — a 500 that tells
    /// the caller not to retry something that is in fact retryable.
    #[error("Service unavailable: {detail}")]
    ServiceUnavailable {
        /// Human-readable diagnostic for operators. NOT surfaced to the
        /// caller — the REST layer logs it and returns a generic envelope.
        detail: String,
        /// Retry hint in seconds, when the producer has a defensible one.
        /// Populates the canonical envelope's `retry_after_seconds` and the
        /// `Retry-After` header.
        retry_after_seconds: Option<u64>,
    },

    /// Catch-all for unexpected failures. Implementations SHOULD prefer a
    /// more specific variant whenever possible.
    #[error("Internal error: {message}")]
    Internal {
        /// Human-readable diagnostic for operators.
        message: String,
    },

    /// A role definition with the same `name` already exists for the same
    /// `owner_tenant_id`. Maps to 409 at the REST surface. Built-in
    /// collisions surface as [`Self::RoleDefinitionNameReservedByBuiltin`].
    #[error("Role definition name already in use: {name}")]
    RoleDefinitionNameTaken {
        /// Conflicting role name.
        name: String,
        /// Owner tenant whose namespace the conflict occurred in
        /// (`None` for built-ins — only used by the seeder path).
        owner_tenant_id: Option<Uuid>,
    },

    /// The requested role name collides (case-insensitively) with a
    /// built-in role name. Maps to 409 at the REST surface.
    #[error("Role definition name '{name}' is reserved by a built-in role")]
    RoleDefinitionNameReservedByBuiltin {
        /// Conflicting role name as supplied by the caller.
        name: String,
    },

    /// A delete was attempted against a role definition that still has
    /// active role assignments. Maps to 400 `failed_precondition` at the
    /// REST surface — unlike the two name-collision variants above, which
    /// really are `already_exists`/409.
    #[error("Role definition {role_definition_id} still has active assignments")]
    RoleDefinitionAssignmentsExist {
        /// Role definition the caller attempted to delete.
        role_definition_id: Uuid,
    },

    /// A `PATCH` or `DELETE` was attempted against a built-in role
    /// definition. Maps to 400 `failed_precondition` at the REST surface —
    /// see `api/rest/error.rs`, whose module doc records that the canonical
    /// taxonomy carries no 409 for this.
    #[error("Role definition {role_definition_id} is built-in and cannot be modified")]
    BuiltInRoleNotModifiable {
        /// Built-in role definition the caller attempted to modify.
        role_definition_id: Uuid,
    },

    /// A request body carried a malformed permission rule. Maps to 400 at
    /// the REST surface. `detail` identifies the offending rule and field.
    #[error("Invalid permission rule: {detail}")]
    InvalidPermissionRule {
        /// Diagnostic identifying the offending rule and field.
        detail: String,
    },

    /// A `PATCH` body carried a field that is immutable post-creation
    /// (`id`, `is_built_in`, `owner_tenant_id`, `created_at`, `created_by`).
    /// Maps to 400 at the REST surface.
    #[error("Immutable field rejected: {field}")]
    ImmutableFieldRejected {
        /// Name of the rejected immutable field.
        field: String,
    },

    /// A tenant-scoped caller supplied an `owner_tenant_id` that does not
    /// match their authentication context. Maps to 403 at the REST surface.
    #[error("Owner tenant mismatch: tenant-scoped callers cannot manage roles in another tenant")]
    OwnerTenantMismatch,

    /// A root-scoped caller omitted `owner_tenant_id` from a create request.
    /// Maps to 400 at the REST surface.
    #[error("Owner tenant required: root-scoped callers MUST supply `owner_tenant_id`")]
    OwnerTenantRequired,

    /// A `PATCH` or `DELETE` request lacked the mandatory `If-Match`
    /// header. Maps to 400 `failed_precondition` with a
    /// `precondition_violations[].type` of `PRECONDITION_REQUIRED`: the
    /// canonical taxonomy does not model 428, so the discriminator is the
    /// violation type rather than the status.
    #[error("Missing If-Match header: optimistic concurrency requires a precondition")]
    OptimisticConcurrencyMissing,

    /// A `PATCH` or `DELETE` request carried an `If-Match` value that did
    /// not byte-match the row's current `ETag`. Maps to 400
    /// `failed_precondition` with a `precondition_violations[].type` of
    /// `PRECONDITION_FAILED` — the taxonomy models no 412 either.
    #[error("Stale If-Match: row has been modified, current `ETag` = {current_etag}")]
    OptimisticConcurrencyStale {
        /// The row's current strong validator at the time of the check.
        current_etag: String,
    },

    /// A `POST /rbac/v1/role-assignments` request violated the `uq_assignment`
    /// uniqueness constraint on
    /// `(role_definition_id, principal_type, principal_id, scope)`. Maps to
    /// 409 at the REST surface.
    ///
    /// `principal_type` is the typed enum, not a `String`. Wire form is
    /// unchanged because `PrincipalType` serializes to the same `PascalCase`
    /// tags (`"User"` / `"Group"` / `"ServicePrincipal"`).
    #[error(
        "Duplicate role assignment: role={role_definition_id}, principal_type={}, principal_id={principal_id}, scope={scope}",
        principal_type.as_str()
    )]
    RoleAssignmentDuplicate {
        /// Role definition being assigned.
        role_definition_id: Uuid,
        /// Principal type for the duplicate row.
        principal_type: PrincipalType,
        /// Opaque principal identifier.
        principal_id: String,
        /// Scope at which the assignment was attempted.
        scope: String,
    },

    /// A `POST /rbac/v1/role-assignments` request supplied a `scope` that does
    /// not fall within the role's `assignable_scopes`. Maps to 400 at the
    /// REST surface.
    #[error("Scope '{scope}' is not within the role's assignable_scopes {assignable_scopes:?}")]
    ScopeNotWithinAssignableScopes {
        /// Requested assignment scope.
        scope: String,
        /// The role's declared assignable scopes.
        assignable_scopes: Vec<String>,
    },

    /// A `POST /rbac/v1/role-assignments` request with `principal_type = Group`
    /// referenced a `principal_id` that does not exist in the
    /// resource-group module. Maps to 404 at the REST surface. A non-UUID
    /// `principal_id` is a structural validation failure surfaced as
    /// [`Self::Validation`] (400), NOT this variant.
    #[error("Group principal not found: {principal_id}")]
    GroupPrincipalNotFound {
        /// Group principal id that failed the RG existence lookup.
        principal_id: Uuid,
    },

    /// A `POST /rbac/v1/role-assignments` request with `principal_type = Group`
    /// targeted the root scope `/`. Maps to 400 at the REST surface.
    #[error("Group principals MUST NOT be assigned at root scope `/`")]
    GroupPrincipalRootScopeForbidden,

    /// A `POST /rbac/v1/role-assignments` request with `principal_type = Group`
    /// targeted a scope whose tenant does not match the group's owning
    /// tenant. Maps to 400 at the REST surface.
    #[error(
        "Group principal tenant mismatch: group's tenant = {group_tenant_id}, scope's tenant = {scope_tenant_id}"
    )]
    GroupPrincipalTenantMismatch {
        /// Tenant id that owns the group (from `hierarchy.tenant_id`).
        group_tenant_id: Uuid,
        /// Tenant id encoded in the assignment scope.
        scope_tenant_id: Uuid,
    },

    /// A `POST /rbac/v1/role-assignments` (or list query) request supplied a
    /// `principal_type` value outside the closed enum `User` / `Group` /
    /// `ServicePrincipal`. Maps to 400 at the REST surface.
    #[error("Invalid principal_type '{value}': must be one of 'User', 'Group', 'ServicePrincipal'")]
    InvalidPrincipalType {
        /// The offending caller-supplied value.
        value: String,
    },

    /// The permission evaluator read a `role_assignments.scope` value that
    /// is syntactically invalid — a state the scope validator prevents on
    /// write. Distinct from `Internal` so corrupted-row diagnostics are
    /// grep-able. No REST surface.
    #[error("Invalid stored scope: '{scope}'")]
    InvalidStoredScope {
        /// The malformed scope string as it was read from the database.
        scope: String,
    },

    /// Aggregated field-level validation failures from a single pass over
    /// the request body. Maps to 400 `invalid_argument` at the REST
    /// surface, with one `context.field_violations` entry per
    /// [`FieldError`]. Single-field validation uses [`Self::Validation`]
    /// and produces the same 400.
    #[error("Validation failed with {} field error(s)", .errors.len())]
    ValidationFailed {
        /// Field-level failures in emit order; order is preserved end-to-end
        /// through serialization.
        errors: Vec<FieldError>,
    },

    /// A request referenced a scope path that does not exist in the tenant
    /// hierarchy or the resource-group module. Maps to 404 at the REST
    /// surface; distinct from [`Self::Validation`] (400) and
    /// [`Self::InvalidScopeFormat`] (422).
    #[error("Scope not found: {scope}")]
    ScopeNotFound {
        /// The scope path that failed the existence lookup.
        scope: String,
    },

    /// A request supplied a scope path that did not parse against the
    /// documented forms (`/`, `/tenants/{uuid}`,
    /// `/tenants/{uuid}/resourceGroups/{uuid}`). Maps to 400
    /// `invalid_argument` at the REST surface, as a single field
    /// violation.
    #[error("Invalid scope format: {scope}")]
    InvalidScopeFormat {
        /// The malformed scope path as supplied by the caller.
        scope: String,
    },

    /// A list request supplied a `limit` outside the allowed range. Maps
    /// to 400 `invalid_argument` at the REST surface, as a single field
    /// violation.
    #[error("Invalid limit: {limit} (max {max})")]
    InvalidLimit {
        /// The offending `limit` value.
        limit: u64,
        /// The maximum allowed `limit` for the endpoint.
        max: u64,
    },

    /// A list request supplied a cursor that did not decode against the
    /// documented opaque format. Maps to 400 at the REST surface per DNA
    /// REST contract.
    #[error("Invalid cursor: {detail}")]
    InvalidCursor {
        /// Diagnostic explaining why the cursor was rejected.
        detail: String,
    },
}

impl RbacServiceError {
    /// Construct a `RoleDefinitionNotFound` error for the given role definition ID.
    #[must_use]
    pub fn role_definition_not_found(id: Uuid) -> Self {
        Self::RoleDefinitionNotFound { id }
    }

    /// Construct a `RoleAssignmentNotFound` error for the given role assignment ID.
    #[must_use]
    pub fn role_assignment_not_found(id: Uuid) -> Self {
        Self::RoleAssignmentNotFound { id }
    }

    /// Construct a `Validation` error with the given message.
    #[must_use]
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation {
            message: message.into(),
        }
    }

    /// Construct an `AuthorizationDenied` error with the given message.
    #[must_use]
    pub fn authorization_denied(message: impl Into<String>) -> Self {
        Self::AuthorizationDenied {
            message: message.into(),
        }
    }

    /// Construct a `Conflict` error with the given message.
    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict {
            message: message.into(),
        }
    }

    /// Construct a `DependencyUnavailable` error with the given dependency tag.
    #[must_use]
    pub fn dependency_unavailable(dependency: impl Into<String>) -> Self {
        Self::DependencyUnavailable {
            dependency: dependency.into(),
        }
    }

    /// Construct a `ServiceUnavailable` error with the given diagnostic
    /// and optional retry hint.
    #[must_use]
    pub fn service_unavailable(
        detail: impl Into<String>,
        retry_after_seconds: Option<u64>,
    ) -> Self {
        Self::ServiceUnavailable {
            detail: detail.into(),
            retry_after_seconds,
        }
    }

    /// Construct an `Internal` error with the given diagnostic.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    /// Construct a `RoleDefinitionNameTaken` error.
    #[must_use]
    pub fn role_definition_name_taken(
        name: impl Into<String>,
        owner_tenant_id: Option<Uuid>,
    ) -> Self {
        Self::RoleDefinitionNameTaken {
            name: name.into(),
            owner_tenant_id,
        }
    }

    /// Construct a `RoleDefinitionNameReservedByBuiltin` error.
    #[must_use]
    pub fn role_definition_name_reserved_by_builtin(name: impl Into<String>) -> Self {
        Self::RoleDefinitionNameReservedByBuiltin { name: name.into() }
    }

    /// Construct a `RoleDefinitionAssignmentsExist` error.
    #[must_use]
    pub fn role_definition_assignments_exist(role_definition_id: Uuid) -> Self {
        Self::RoleDefinitionAssignmentsExist { role_definition_id }
    }

    /// Construct a `BuiltInRoleNotModifiable` error.
    #[must_use]
    pub fn built_in_role_not_modifiable(role_definition_id: Uuid) -> Self {
        Self::BuiltInRoleNotModifiable { role_definition_id }
    }

    /// Construct an `InvalidPermissionRule` error.
    #[must_use]
    pub fn invalid_permission_rule(detail: impl Into<String>) -> Self {
        Self::InvalidPermissionRule {
            detail: detail.into(),
        }
    }

    /// Construct an `ImmutableFieldRejected` error.
    #[must_use]
    pub fn immutable_field_rejected(field: impl Into<String>) -> Self {
        Self::ImmutableFieldRejected {
            field: field.into(),
        }
    }

    /// Construct an `OwnerTenantMismatch` error.
    #[must_use]
    pub fn owner_tenant_mismatch() -> Self {
        Self::OwnerTenantMismatch
    }

    /// Construct an `OwnerTenantRequired` error.
    #[must_use]
    pub fn owner_tenant_required() -> Self {
        Self::OwnerTenantRequired
    }

    /// Construct an `OptimisticConcurrencyMissing` error.
    #[must_use]
    pub fn optimistic_concurrency_missing() -> Self {
        Self::OptimisticConcurrencyMissing
    }

    /// Construct an `OptimisticConcurrencyStale` error.
    #[must_use]
    pub fn optimistic_concurrency_stale(current_etag: impl Into<String>) -> Self {
        Self::OptimisticConcurrencyStale {
            current_etag: current_etag.into(),
        }
    }

    /// Construct a `RoleAssignmentDuplicate` error.
    #[must_use]
    pub fn role_assignment_duplicate(
        role_definition_id: Uuid,
        principal_type: PrincipalType,
        principal_id: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        Self::RoleAssignmentDuplicate {
            role_definition_id,
            principal_type,
            principal_id: principal_id.into(),
            scope: scope.into(),
        }
    }

    /// Construct a `ScopeNotWithinAssignableScopes` error.
    #[must_use]
    pub fn scope_not_within_assignable_scopes(
        scope: impl Into<String>,
        assignable_scopes: Vec<String>,
    ) -> Self {
        Self::ScopeNotWithinAssignableScopes {
            scope: scope.into(),
            assignable_scopes,
        }
    }

    /// Construct a `GroupPrincipalNotFound` error.
    #[must_use]
    pub fn group_principal_not_found(principal_id: Uuid) -> Self {
        Self::GroupPrincipalNotFound { principal_id }
    }

    /// Construct a `GroupPrincipalRootScopeForbidden` error.
    #[must_use]
    pub fn group_principal_root_scope_forbidden() -> Self {
        Self::GroupPrincipalRootScopeForbidden
    }

    /// Construct a `GroupPrincipalTenantMismatch` error.
    #[must_use]
    pub fn group_principal_tenant_mismatch(group_tenant_id: Uuid, scope_tenant_id: Uuid) -> Self {
        Self::GroupPrincipalTenantMismatch {
            group_tenant_id,
            scope_tenant_id,
        }
    }

    /// Construct an `InvalidPrincipalType` error.
    #[must_use]
    pub fn invalid_principal_type(value: impl Into<String>) -> Self {
        Self::InvalidPrincipalType {
            value: value.into(),
        }
    }

    /// Construct an `InvalidStoredScope` error from the bad scope string.
    #[must_use]
    pub fn invalid_stored_scope(scope: impl Into<String>) -> Self {
        Self::InvalidStoredScope {
            scope: scope.into(),
        }
    }

    /// Construct a `ValidationFailed` error. The accumulated list is capped
    /// at [`MAX_FIELD_ERRORS`] via a sentinel `FieldError`.
    #[must_use]
    pub fn validation_failed(errors: Vec<FieldError>) -> Self {
        Self::ValidationFailed {
            errors: cap_field_errors(errors),
        }
    }

    /// Construct a `ScopeNotFound` error for the given scope path.
    #[must_use]
    pub fn scope_not_found(scope: impl Into<String>) -> Self {
        Self::ScopeNotFound {
            scope: scope.into(),
        }
    }

    /// Construct an `InvalidScopeFormat` error for the given scope path.
    #[must_use]
    pub fn invalid_scope_format(scope: impl Into<String>) -> Self {
        Self::InvalidScopeFormat {
            scope: scope.into(),
        }
    }

    /// Construct an `InvalidLimit` error for the given offending limit.
    #[must_use]
    pub fn invalid_limit(limit: u64, max: u64) -> Self {
        Self::InvalidLimit { limit, max }
    }

    /// Construct an `InvalidCursor` error with the given diagnostic.
    #[must_use]
    pub fn invalid_cursor(detail: impl Into<String>) -> Self {
        Self::InvalidCursor {
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
