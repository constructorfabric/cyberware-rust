//! Wire-shape tests for `rbac_service_error_to_canonical`. Pins the
//! status code and resource context the canonical-error framework
//! produces — that is the contract handlers and middleware live by.

use rbac_sdk::error::{FieldError, RbacServiceError};
use toolkit::api::canonical_prelude::Problem;
use uuid::Uuid;

use super::{ResourceKind, rbac_service_error_to_canonical, rbac_service_error_to_canonical_for};

fn problem_from(err: RbacServiceError) -> Problem {
    Problem::from(rbac_service_error_to_canonical(err))
}

fn problem_from_for(err: RbacServiceError, resource: ResourceKind) -> Problem {
    Problem::from(rbac_service_error_to_canonical_for(err, resource))
}

// ---------------------------------------------------------------------------
// Status-code coverage
// ---------------------------------------------------------------------------

#[test]
fn not_found_variants_map_to_404() {
    let id = Uuid::nil();
    assert_eq!(
        problem_from(RbacServiceError::RoleDefinitionNotFound { id }).status,
        Some(404)
    );
    assert_eq!(
        problem_from(RbacServiceError::RoleAssignmentNotFound { id }).status,
        Some(404)
    );
    assert_eq!(
        problem_from(RbacServiceError::scope_not_found("/tenants/x")).status,
        Some(404)
    );
    assert_eq!(
        problem_from(RbacServiceError::group_principal_not_found(id)).status,
        Some(404)
    );
}

#[test]
fn invalid_argument_variants_map_to_400() {
    // `Validation` and every single-field short-circuit collapse to
    // `InvalidArgument` (HTTP 400) in the canonical taxonomy.
    let id = Uuid::nil();
    let cases: Vec<RbacServiceError> = vec![
        RbacServiceError::validation("bad"),
        RbacServiceError::invalid_permission_rule("oops"),
        RbacServiceError::immutable_field_rejected("is_built_in"),
        RbacServiceError::owner_tenant_required(),
        RbacServiceError::scope_not_within_assignable_scopes(
            "/tenants/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            vec!["/tenants/bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_owned()],
        ),
        RbacServiceError::group_principal_root_scope_forbidden(),
        RbacServiceError::group_principal_tenant_mismatch(id, id),
        RbacServiceError::invalid_scope_format("/x"),
        RbacServiceError::invalid_limit(500, 200),
        RbacServiceError::invalid_cursor("base64"),
        RbacServiceError::invalid_principal_type("Robot"),
        RbacServiceError::validation_failed(vec![FieldError::new("a", "x", "empty")]),
    ];
    for err in cases {
        let p = problem_from(err);
        assert_eq!(
            p.status,
            Some(400),
            "expected 400 for InvalidArgument category; got {p:?}"
        );
    }
}

#[test]
fn permission_denied_variants_map_to_403_and_do_not_leak_internal_message() {
    let p = problem_from(RbacServiceError::authorization_denied(
        "caller lacks Owner on /tenants/acme",
    ));
    assert_eq!(p.status, Some(403));
    // Serialize the entire problem (detail + title + context) and
    // assert no part of the leaked-message substring appears anywhere
    // on the wire — `p.detail` alone is not the full attack surface.
    let body = serde_json::to_string(&p).expect("Problem serializes as JSON");
    assert!(
        !body.contains("acme"),
        "internal authz message must not leak into any wire field: {body}"
    );

    let p = problem_from(RbacServiceError::owner_tenant_mismatch());
    assert_eq!(p.status, Some(403));
}

#[test]
fn already_exists_variants_map_to_409() {
    let id = Uuid::nil();
    assert_eq!(
        problem_from(RbacServiceError::conflict("dup")).status,
        Some(409)
    );
    assert_eq!(
        problem_from(RbacServiceError::role_definition_name_taken(
            "Auditor",
            Some(id)
        ))
        .status,
        Some(409)
    );
    assert_eq!(
        problem_from(RbacServiceError::role_definition_name_reserved_by_builtin(
            "Owner"
        ))
        .status,
        Some(409)
    );
    assert_eq!(
        problem_from(RbacServiceError::role_assignment_duplicate(
            id,
            rbac_sdk::models::PrincipalType::User,
            "alice",
            "/tenants/x"
        ))
        .status,
        Some(409)
    );
}

/// `RoleDefinitionAssignmentsExist` and `BuiltInRoleNotModifiable` are
/// precondition failures, not duplicate-create failures, so they map to
/// `400 FailedPrecondition` rather than `409 AlreadyExists`. Callers dispatch
/// on `400` plus `context.precondition_violations[].type`
/// (`BUILT_IN_ROLE_NOT_MODIFIABLE` / `ROLE_DEFINITION_HAS_ASSIGNMENTS`).
#[test]
fn precondition_failures_for_role_definition_lifecycle_map_to_400() {
    let id = Uuid::nil();
    assert_eq!(
        problem_from(RbacServiceError::role_definition_assignments_exist(id)).status,
        Some(400)
    );
    assert_eq!(
        problem_from(RbacServiceError::built_in_role_not_modifiable(id)).status,
        Some(400)
    );
}

#[test]
fn failed_precondition_variants_map_to_400() {
    // Canonical taxonomy collapses 428 / 412 into FailedPrecondition
    // (HTTP 400). Callers branch on `context.violations[].type`
    // (PRECONDITION_REQUIRED vs PRECONDITION_FAILED).
    assert_eq!(
        problem_from(RbacServiceError::optimistic_concurrency_missing()).status,
        Some(400)
    );
    assert_eq!(
        problem_from(RbacServiceError::optimistic_concurrency_stale("etag")).status,
        Some(400)
    );
}

#[test]
fn dependency_unavailable_maps_to_503() {
    assert_eq!(
        problem_from(RbacServiceError::dependency_unavailable(
            "TenantResolverClient"
        ))
        .status,
        Some(503)
    );
}

#[test]
fn service_unavailable_maps_to_503_without_leaking_diagnostic() {
    let p = problem_from(RbacServiceError::service_unavailable(
        "tenant resolver connection refused",
        Some(30),
    ));
    assert_eq!(
        p.status,
        Some(503),
        "a transient outage must be a retryable 503, not a 500"
    );
    assert!(
        !p.detail.contains("connection refused"),
        "the operator diagnostic must stay in the log, not the response detail"
    );
}

#[test]
fn internal_maps_to_500_without_leaking_diagnostic() {
    let p = problem_from(RbacServiceError::internal("pg connection pool exhausted"));
    assert_eq!(p.status, Some(500));
    assert!(
        !p.detail.contains("pg connection"),
        "internal diagnostic must not leak into the response detail"
    );
}

// ---------------------------------------------------------------------------
// Resource correlation
// ---------------------------------------------------------------------------

#[test]
fn role_definition_not_found_carries_resource_context() {
    let id = Uuid::nil();
    let p = problem_from(RbacServiceError::RoleDefinitionNotFound { id });
    assert_eq!(
        p.context["resource_type"],
        "gts.cf.core.rbac.role_definition.v1~"
    );
    assert_eq!(p.context["resource_name"], id.to_string());
}

#[test]
fn role_assignment_not_found_carries_resource_context() {
    let id = Uuid::nil();
    let p = problem_from(RbacServiceError::RoleAssignmentNotFound { id });
    assert_eq!(
        p.context["resource_type"],
        "gts.cf.core.rbac.role_assignment.v1~"
    );
    assert_eq!(p.context["resource_name"], id.to_string());
}

/// Generic error arms must stamp the *caller's* resource type,
/// not the hardwired role-definition default. Pins that
/// `rbac_service_error_to_canonical_for(_, RoleAssignment)` routes a
/// generic `Validation` (single-field) and a multi-field
/// `ValidationFailed` through the role-assignment factory.
#[test]
fn generic_validation_arms_stamp_assignment_resource_when_requested() {
    // single-field arm (InvalidScopeFormat → single_field_invalid_argument)
    let p = problem_from_for(
        RbacServiceError::invalid_scope_format("/bad"),
        ResourceKind::RoleAssignment,
    );
    assert_eq!(
        p.context["resource_type"], "gts.cf.core.rbac.role_assignment.v1~",
        "single-field validation on an assignment endpoint MUST stamp the role-assignment resource"
    );

    // multi-field arm (ValidationFailed → multi_field_invalid_argument)
    let p = problem_from_for(
        RbacServiceError::ValidationFailed {
            errors: vec![FieldError {
                field: "principal_id".to_owned(),
                message: "must be non-empty".to_owned(),
                code: "REQUIRED".to_owned(),
            }],
        },
        ResourceKind::RoleAssignment,
    );
    assert_eq!(
        p.context["resource_type"], "gts.cf.core.rbac.role_assignment.v1~",
        "multi-field validation on an assignment endpoint MUST stamp the role-assignment resource"
    );
}

/// Default (`rbac_service_error_to_canonical`) stamps role-definition for the
/// generic arms, which is what the role-definition handlers rely on.
#[test]
fn generic_validation_arms_default_to_role_definition_resource() {
    let p = problem_from(RbacServiceError::invalid_scope_format("/bad"));
    assert_eq!(
        p.context["resource_type"],
        "gts.cf.core.rbac.role_definition.v1~"
    );
}

/// The generic `Validation` and `AuthorizationDenied` arms also
/// branch on the caller's resource (`match resource` in the mapper).
/// The multi-field test alone leaves them unpinned, so pin them here too:
/// the "always stamps role-definition" regression class must not reappear
/// on a role-assignment endpoint.
#[test]
fn validation_and_authz_denied_arms_stamp_assignment_resource_when_requested() {
    let p = problem_from_for(
        RbacServiceError::validation("bad assignment"),
        ResourceKind::RoleAssignment,
    );
    assert_eq!(p.status, Some(400));
    assert_eq!(
        p.context["resource_type"], "gts.cf.core.rbac.role_assignment.v1~",
        "generic Validation on an assignment endpoint MUST stamp the role-assignment resource"
    );

    let p = problem_from_for(
        RbacServiceError::authorization_denied("caller lacks write on /tenants/acme"),
        ResourceKind::RoleAssignment,
    );
    assert_eq!(p.status, Some(403));
    assert_eq!(
        p.context["resource_type"], "gts.cf.core.rbac.role_assignment.v1~",
        "AuthorizationDenied on an assignment endpoint MUST stamp the role-assignment resource"
    );
    let body = serde_json::to_string(&p).expect("Problem serializes as JSON");
    assert!(
        !body.contains("acme"),
        "internal authz message must not leak into any wire field: {body}"
    );
}

// ---------------------------------------------------------------------------
// Field-violation context
// ---------------------------------------------------------------------------

#[test]
fn single_field_short_circuit_emits_one_field_violation() {
    let p = problem_from(RbacServiceError::owner_tenant_required());
    let violations = p
        .context
        .get("field_violations")
        .and_then(|v| v.as_array())
        .expect("InvalidArgument with single-field short-circuit must emit field_violations");
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["field"], "owner_tenant_id");
    assert_eq!(violations[0]["reason"], "owner_tenant_required");
}

#[test]
fn multi_field_validation_emits_ordered_field_violations() {
    let err = RbacServiceError::validation_failed(vec![
        FieldError::new("a", "x", "empty"),
        FieldError::new("b", "y", "format"),
    ]);
    let p = problem_from(err);
    let violations = p
        .context
        .get("field_violations")
        .and_then(|v| v.as_array())
        .expect("validation_failed must emit field_violations");
    assert_eq!(violations.len(), 2);
    assert_eq!(violations[0]["field"], "a");
    assert_eq!(violations[0]["description"], "x");
    assert_eq!(violations[0]["reason"], "empty");
    assert_eq!(violations[1]["field"], "b");
    assert_eq!(violations[1]["reason"], "format");
}

// ---------------------------------------------------------------------------
// Precondition violations carry the type marker
// ---------------------------------------------------------------------------

#[test]
fn optimistic_concurrency_missing_carries_precondition_required_marker() {
    let p = problem_from(RbacServiceError::optimistic_concurrency_missing());
    let violations = p
        .context
        .get("violations")
        .and_then(|v| v.as_array())
        .expect("FailedPrecondition must emit violations");
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["type"], "PRECONDITION_REQUIRED");
    assert_eq!(violations[0]["subject"], "If-Match");
}

#[test]
fn optimistic_concurrency_stale_carries_precondition_failed_marker_and_etag() {
    let p = problem_from(RbacServiceError::optimistic_concurrency_stale("E_current"));
    let violations = p
        .context
        .get("violations")
        .and_then(|v| v.as_array())
        .expect("FailedPrecondition must emit violations");
    assert_eq!(violations[0]["type"], "PRECONDITION_FAILED");
    assert!(
        violations[0]["description"]
            .as_str()
            .unwrap()
            .contains("E_current"),
        "stale-ETag description must echo the current server-side ETag"
    );
}
