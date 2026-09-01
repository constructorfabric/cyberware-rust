//! REST error mapping for the `rbac` module: translates `RbacServiceError`
//! into `CanonicalError`, which the canonical-error middleware renders as
//! an RFC 9457 `Problem`.
//!
//! ## Wire-shape notes
//!
//! * SDK consumers branch on `category` + `context.resource_type`.
//! * `OptimisticConcurrencyMissing` / `OptimisticConcurrencyStale` map to
//!   `FailedPrecondition` (HTTP 400) — the canonical taxonomy does not
//!   model 412 / 428. Callers distinguish the two via
//!   `precondition_violations[].type`
//!   (`PRECONDITION_REQUIRED` vs `PRECONDITION_FAILED`).
//! * Multi-field validation (`ValidationFailed`)
//!   renders into `context.field_violations`: each `FieldError` becomes a
//!   `FieldViolation { field, description = FieldError.message,
//!   reason = FieldError.code }`.
//! * `Internal` and `DependencyUnavailable` log their diagnostic
//!   server-side; internal text never leaks to callers.

use rbac_sdk::error::{FieldError, FieldViolationField, FieldViolationReason, RbacServiceError};
use toolkit::api::canonical_prelude::{CanonicalError, resource_error};

use crate::domain::error::DomainError;

/// Compose `DomainError` → `RbacServiceError` → `CanonicalError`. REST
/// handlers that hold a raw `DomainError` (e.g. the few code paths that
/// short-circuit before lifting to the SDK enum) can lean on `?` to
/// produce the canonical envelope directly. The SDK boundary still owns
/// the variant table; this impl is the convenience hop.
impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        rbac_service_error_to_canonical(RbacServiceError::from(err))
    }
}

// ---------------------------------------------------------------------------
// Resource-scoped canonical error markers. `#[resource_error]` emits
// constructors that stamp the resource's GTS identifier into the canonical
// error's `context.resource_type` block.
// ---------------------------------------------------------------------------

#[resource_error(gts_id!("cf.core.rbac.role_definition.v1~"))]
struct RoleDefinitionResourceError;

#[resource_error(gts_id!("cf.core.rbac.role_assignment.v1~"))]
struct RoleAssignmentResourceError;

/// Per-handler resource context for the canonical-error
/// mapper. Threaded through `rbac_service_error_to_canonical_for` so
/// the generic arms (`Validation`, `Conflict`, `AuthorizationDenied`,
/// `OptimisticConcurrency*`, `Internal`) stamp the right
/// `context.resource_type` block. The resource-specific arms
/// (`RoleAssignmentDuplicate`, `RoleDefinitionNameTaken`, …) stay
/// hardwired to their owning factory regardless of this parameter.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ResourceKind {
    RoleDefinition,
    RoleAssignment,
}

// ---------------------------------------------------------------------------
// RbacServiceError → CanonicalError
// ---------------------------------------------------------------------------

/// Translate an [`RbacServiceError`] into a [`CanonicalError`] in the
/// role-definition context. Convenience wrapper around
/// [`rbac_service_error_to_canonical_for`] — handlers that own
/// role-assignment endpoints SHOULD use the `_for(..., ResourceKind::RoleAssignment)`
/// path so generic-arm errors (`Validation`, `Conflict`,
/// `AuthorizationDenied`, `OptimisticConcurrency*`, `Internal`) stamp
/// the right `context.resource_type`.
pub(crate) fn rbac_service_error_to_canonical(err: RbacServiceError) -> CanonicalError {
    rbac_service_error_to_canonical_for(err, ResourceKind::RoleDefinition)
}

/// Translate an [`RbacServiceError`] into a [`CanonicalError`] in the
/// resource context the calling handler owns. Used as
/// `.map_err(|e| rbac_service_error_to_canonical_for(e, ResourceKind::RoleAssignment))?`
/// in assignment-endpoint handlers so SDK clients branching on
/// `context.resource_type` see the right `gts.cf.core.rbac.role_assignment.v1~`
/// instead of `…role_definition…`.
///
/// The orphan rule blocks a direct `From` impl on `CanonicalError` so
/// this function exists as a free helper.
// Single, flat `match` on every `RbacServiceError` variant: the size is
// the point — one arm per variant keeps the contract review-able in one
// place, so the `too_many_lines` and `cognitive_complexity` caps are
// suppressed here intentionally. Both are driven by the arm count, and
// splitting the table would scatter the wire contract across helpers
// without removing a single branch.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub(crate) fn rbac_service_error_to_canonical_for(
    err: RbacServiceError,
    resource: ResourceKind,
) -> CanonicalError {
    match err {
            // ---- Generic resource lookups (404) ----
            RbacServiceError::RoleDefinitionNotFound { id } => {
                RoleDefinitionResourceError::not_found(format!(
                    "Role definition '{id}' was not found"
                ))
                .with_resource(id.to_string())
                .create()
            }
            RbacServiceError::RoleAssignmentNotFound { id } => {
                RoleAssignmentResourceError::not_found(format!(
                    "Role assignment '{id}' was not found"
                ))
                .with_resource(id.to_string())
                .create()
            }
            RbacServiceError::ScopeNotFound { scope } => {
                RoleAssignmentResourceError::not_found(format!("Scope '{scope}' was not found"))
                    .with_resource(scope.as_str())
                    .create()
            }
            RbacServiceError::GroupPrincipalNotFound { principal_id } => {
                RoleAssignmentResourceError::not_found(format!(
                    "Group principal '{principal_id}' was not found"
                ))
                .with_resource(principal_id.to_string())
                .create()
            }

            // ---- Generic validation (400 / InvalidArgument) ----
            //
            // Generic arms stamp the right `context.resource_type` via
            // the per-handler `resource` parameter so SDK clients
            // branching on `resource_type` see e.g.
            // `…role_assignment…` for an assignment-endpoint validation
            // failure instead of always `…role_definition…`.
            RbacServiceError::Validation { message } => match resource {
                ResourceKind::RoleDefinition => RoleDefinitionResourceError::invalid_argument()
                    .with_constraint(message)
                    .create(),
                ResourceKind::RoleAssignment => RoleAssignmentResourceError::invalid_argument()
                    .with_constraint(message)
                    .create(),
            },
            RbacServiceError::InvalidPermissionRule { detail } => {
                single_field_invalid_argument(
                    resource,
                    FieldViolationField::Permissions.as_str(),
                    detail,
                    FieldViolationReason::InvalidPermissionRule,
                )
            }
            RbacServiceError::ImmutableFieldRejected { field } => {
                let message = format!("field '{field}' is immutable and cannot be set on PATCH");
                single_field_invalid_argument(
                    resource,
                    field,
                    message,
                    FieldViolationReason::ImmutableFieldRejected,
                )
            }
            RbacServiceError::OwnerTenantRequired => single_field_invalid_argument(
                resource,
                FieldViolationField::OwnerTenantId.as_str(),
                "owner_tenant_id is required for root-scoped callers",
                FieldViolationReason::OwnerTenantRequired,
            ),
            RbacServiceError::ScopeNotWithinAssignableScopes {
                scope,
                assignable_scopes,
            } => single_field_invalid_argument(
                resource,
                FieldViolationField::Scope.as_str(),
                format!(
                    "scope '{scope}' is not within the role's assignable_scopes {assignable_scopes:?}"
                ),
                FieldViolationReason::ScopeNotWithinAssignableScopes,
            ),
            RbacServiceError::GroupPrincipalRootScopeForbidden => single_field_invalid_argument(
                resource,
                FieldViolationField::Scope.as_str(),
                "group principals MUST NOT be assigned at root scope `/`",
                FieldViolationReason::GroupPrincipalRootScopeForbidden,
            ),
            RbacServiceError::GroupPrincipalTenantMismatch {
                group_tenant_id,
                scope_tenant_id,
            } => single_field_invalid_argument(
                resource,
                FieldViolationField::Scope.as_str(),
                format!(
                    "group tenant mismatch: group's tenant = {group_tenant_id}, scope's tenant = {scope_tenant_id}"
                ),
                FieldViolationReason::GroupPrincipalTenantMismatch,
            ),
            RbacServiceError::InvalidScopeFormat { scope } => single_field_invalid_argument(
                resource,
                FieldViolationField::Scope.as_str(),
                format!(
                    "scope '{scope}' does not parse as `/`, `/tenants/{{uuid}}`, or `/tenants/{{uuid}}/resourceGroups/{{uuid}}`"
                ),
                FieldViolationReason::InvalidScopeFormat,
            ),
            RbacServiceError::InvalidLimit { limit, max } => single_field_invalid_argument(
                resource,
                FieldViolationField::Limit.as_str(),
                format!("limit {limit} exceeds the maximum allowed value {max}"),
                FieldViolationReason::InvalidLimit,
            ),
            RbacServiceError::InvalidCursor { detail } => single_field_invalid_argument(
                resource,
                FieldViolationField::Cursor.as_str(),
                format!("invalid cursor: {detail}"),
                FieldViolationReason::InvalidCursor,
            ),
            RbacServiceError::InvalidPrincipalType { value } => single_field_invalid_argument(
                resource,
                FieldViolationField::PrincipalType.as_str(),
                format!(
                    "invalid principal_type '{value}': must be one of 'User', 'Group', 'ServicePrincipal'"
                ),
                FieldViolationReason::InvalidPrincipalType,
            ),

            // ---- Multi-field validation (400 / InvalidArgument) ----
            RbacServiceError::ValidationFailed { errors } => {
                multi_field_invalid_argument(&errors, resource)
            }

            // ---- Authorisation (403 / PermissionDenied) ----
            //
            // The domain message is intentionally not forwarded to the
            // wire response to avoid leaking RBAC policy details. The
            // per-handler `resource` decides which resource type the
            // denial is stamped with, so an assignment-endpoint denial
            // is not reported as a role-definition error.
            RbacServiceError::AuthorizationDenied { .. } => match resource {
                ResourceKind::RoleDefinition => RoleDefinitionResourceError::permission_denied()
                    .with_reason("ACCESS_DENIED")
                    .create(),
                ResourceKind::RoleAssignment => RoleAssignmentResourceError::permission_denied()
                    .with_reason("ACCESS_DENIED")
                    .create(),
            },
            RbacServiceError::OwnerTenantMismatch => match resource {
                ResourceKind::RoleDefinition => RoleDefinitionResourceError::permission_denied()
                    .with_reason("OWNER_TENANT_MISMATCH")
                    .create(),
                ResourceKind::RoleAssignment => RoleAssignmentResourceError::permission_denied()
                    .with_reason("OWNER_TENANT_MISMATCH")
                    .create(),
            },

            // ---- Conflicts (409 / AlreadyExists) ----
            //
            // Generic `Conflict` has no stable identifier — the message
            // is free-form prose. Use an empty `resource_name` rather
            // than echoing the diagnostic text so SDK clients branching
            // on `context.resource_name` don't receive prose.
            //
            // Per-handler resource: a Conflict raised by an
            // assignment-endpoint handler now stamps
            // `…role_assignment…` instead of always `…role_definition…`.
            RbacServiceError::Conflict { message } => match resource {
                ResourceKind::RoleDefinition => RoleDefinitionResourceError::already_exists(message)
                    .with_resource(String::new())
                    .create(),
                ResourceKind::RoleAssignment => RoleAssignmentResourceError::already_exists(message)
                    .with_resource(String::new())
                    .create(),
            },
            RbacServiceError::RoleDefinitionNameTaken { name, .. } => {
                RoleDefinitionResourceError::already_exists(format!(
                    "role definition name '{name}' is already in use"
                ))
                .with_resource(name)
                .create()
            }
            RbacServiceError::RoleDefinitionNameReservedByBuiltin { name } => {
                RoleDefinitionResourceError::already_exists(format!(
                    "role definition name '{name}' is reserved by a built-in role"
                ))
                .with_resource(name)
                .create()
            }
            // Precondition failures, not duplicate-create. Wire
            // status moves from `409 AlreadyExists` to `400
            // FailedPrecondition`; clients branching on canonical
            // category now see the right business meaning. The
            // existing 409 carried the wrong category for SDK
            // dispatch — clients had to special-case the "you can't
            // delete a role with active assignments" /
            // "built-ins are immutable" messages.
            RbacServiceError::RoleDefinitionAssignmentsExist { role_definition_id } => {
                RoleDefinitionResourceError::failed_precondition()
                    .with_precondition_violation(
                        format!("role-definition:{role_definition_id}"),
                        format!(
                            "role definition '{role_definition_id}' still has active assignments"
                        ),
                        "ROLE_DEFINITION_HAS_ASSIGNMENTS",
                    )
                    .create()
            }
            RbacServiceError::BuiltInRoleNotModifiable { role_definition_id } => {
                RoleDefinitionResourceError::failed_precondition()
                    .with_precondition_violation(
                        format!("role-definition:{role_definition_id}"),
                        format!(
                            "role definition '{role_definition_id}' is built-in and cannot be modified"
                        ),
                        "BUILT_IN_ROLE_NOT_MODIFIABLE",
                    )
                    .create()
            }
            RbacServiceError::RoleAssignmentDuplicate {
                role_definition_id,
                principal_type,
                principal_id,
                scope,
            } => RoleAssignmentResourceError::already_exists(format!(
                "role assignment already exists: role={role_definition_id}, principal_type={principal_type}, principal_id={principal_id}, scope={scope}"
            ))
            .with_resource(format!(
                "{role_definition_id}:{principal_type}:{principal_id}:{scope}"
            ))
            .create(),

            // ---- Optimistic concurrency (400 / FailedPrecondition) ----
            //
            // Canonical taxonomy collapses 428 / 412 into FailedPrecondition
            // (400). Callers branch on precondition_violations[].type.
            RbacServiceError::OptimisticConcurrencyMissing => match resource {
                ResourceKind::RoleDefinition => RoleDefinitionResourceError::failed_precondition()
                    .with_precondition_violation(
                        "If-Match",
                        "Required for optimistic concurrency on PATCH/DELETE",
                        "PRECONDITION_REQUIRED",
                    )
                    .create(),
                ResourceKind::RoleAssignment => RoleAssignmentResourceError::failed_precondition()
                    .with_precondition_violation(
                        "If-Match",
                        "Required for optimistic concurrency on PATCH/DELETE",
                        "PRECONDITION_REQUIRED",
                    )
                    .create(),
            },
            RbacServiceError::OptimisticConcurrencyStale { current_etag } => match resource {
                ResourceKind::RoleDefinition => RoleDefinitionResourceError::failed_precondition()
                    .with_precondition_violation(
                        "If-Match",
                        format!("If-Match did not match current ETag {current_etag}"),
                        "PRECONDITION_FAILED",
                    )
                    .create(),
                ResourceKind::RoleAssignment => RoleAssignmentResourceError::failed_precondition()
                    .with_precondition_violation(
                        "If-Match",
                        format!("If-Match did not match current ETag {current_etag}"),
                        "PRECONDITION_FAILED",
                    )
                    .create(),
            },

            // ---- Upstream / infrastructure failures ----
            RbacServiceError::DependencyUnavailable { dependency } => {
                tracing::warn!(
                    dependency = %dependency,
                    "RBAC dependency unavailable; returning 503"
                );
                CanonicalError::service_unavailable().create()
            }
            RbacServiceError::ServiceUnavailable {
                detail,
                retry_after_seconds,
            } => {
                tracing::warn!(
                    diagnostic = %detail,
                    retry_after_seconds = ?retry_after_seconds,
                    "RBAC transient outage; returning 503"
                );
                // `detail` is an operator diagnostic and stays in the log:
                // the envelope carries only the retry hint, matching how
                // the `Internal` arm below withholds its diagnostic.
                let error = CanonicalError::service_unavailable();
                match retry_after_seconds {
                    Some(seconds) => error.with_retry_after_seconds(seconds).create(),
                    None => error.create(),
                }
            }
            RbacServiceError::Internal { message } => {
                tracing::error!(
                    diagnostic = %message,
                    "RBAC internal error occurred"
                );
                CanonicalError::internal("An internal error occurred. Please retry later.").create()
            }

            // `RbacServiceError` is `#[non_exhaustive]`; unmapped variants
            // fall back to a 500 Internal and log the variant name so
            // operators can pinpoint the SDK upgrade that needs follow-up.
            unknown => {
                tracing::error!(
                    variant = ?unknown,
                    "RBAC service error variant not mapped to a canonical category; \
                     the rbac REST mapper needs to be updated to cover this variant"
                );
                CanonicalError::internal("An internal error occurred. Please retry later.").create()
            }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an `InvalidArgument` `CanonicalError` carrying a single
/// `FieldViolation` so single-field failures share the
/// `context.field_violations` shape with multi-field failures.
///
/// `field` accepts both typed [`FieldViolationField`] values (via their
/// `as_str()`) and runtime strings — `ImmutableFieldRejected` carries a
/// dynamic field name decided by the caller, so the field slot cannot be
/// constrained to the enum. `reason` is always known at compile time and
/// uses the typed [`FieldViolationReason`].
///
/// `resource` selects the canonical `resource_type` block, so a validation
/// failure on an assignment endpoint is not stamped as a role-definition error.
/// Both `*ResourceError::invalid_argument()` factories return the same builder
/// type, so the branch only swaps the embedded GTS id.
fn single_field_invalid_argument(
    resource: ResourceKind,
    field: impl Into<String>,
    description: impl Into<String>,
    reason: FieldViolationReason,
) -> CanonicalError {
    let base = match resource {
        ResourceKind::RoleDefinition => RoleDefinitionResourceError::invalid_argument(),
        ResourceKind::RoleAssignment => RoleAssignmentResourceError::invalid_argument(),
    };
    base.with_field_violation(field, description, reason.as_str())
        .create()
}

/// Build an `InvalidArgument` `CanonicalError` carrying every
/// `FieldError` as a `FieldViolation` entry. Maps `FieldError.message`
/// to `FieldViolation.description` and `FieldError.code` to
/// `FieldViolation.reason` (the canonical context has no separate
/// `code` slot).
///
/// `resource` selects the canonical `resource_type` block.
fn multi_field_invalid_argument(errors: &[FieldError], resource: ResourceKind) -> CanonicalError {
    let base = match resource {
        ResourceKind::RoleDefinition => RoleDefinitionResourceError::invalid_argument(),
        ResourceKind::RoleAssignment => RoleAssignmentResourceError::invalid_argument(),
    };
    let mut iter = errors.iter();
    let Some(first) = iter.next() else {
        return base.with_constraint("validation failed").create();
    };
    let mut builder = base.with_field_violation(
        first.field.clone(),
        first.message.clone(),
        first.code.clone(),
    );
    for e in iter {
        builder = builder.with_field_violation(e.field.clone(), e.message.clone(), e.code.clone());
    }
    builder.create()
}

/// Build a 401 `CanonicalError` for requests without an authenticated
/// `SecurityContext` — distinct from `AuthorizationDenied` (403).
pub(crate) fn unauthenticated_error() -> CanonicalError {
    CanonicalError::unauthenticated()
        .with_reason("AUTHENTICATION_REQUIRED")
        .create()
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
