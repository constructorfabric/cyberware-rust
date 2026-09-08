//! Tests for [`super::RbacServiceError`] constructors, displays, and the
//! field-error truncation invariant.

#![allow(clippy::expect_used)]

use uuid::Uuid;

use super::{FieldError, MAX_FIELD_ERRORS, RbacServiceError, TRUNCATION_SENTINEL_CODE};

fn round_trip<T>(value: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let bytes = serde_json::to_vec(value).expect("serialize");
    let back: T = serde_json::from_slice(&bytes).expect("deserialize");
    assert_eq!(value, &back, "round-trip must be lossless");
    back
}

#[test]
fn role_definition_not_found_displays_id() {
    let id = Uuid::nil();
    let err = RbacServiceError::RoleDefinitionNotFound { id };
    assert!(err.to_string().contains(&id.to_string()));
    assert!(err.to_string().starts_with("Role definition not found"));
}

#[test]
fn role_assignment_not_found_displays_id() {
    let id = Uuid::nil();
    let err = RbacServiceError::RoleAssignmentNotFound { id };
    assert!(err.to_string().contains(&id.to_string()));
    assert!(err.to_string().starts_with("Role assignment not found"));
}

#[test]
fn validation_constructor_produces_validation_variant() {
    let err = RbacServiceError::validation("bad input");
    assert!(matches!(err, RbacServiceError::Validation { .. }));
    assert!(err.to_string().contains("bad input"));
}

#[test]
fn authorization_denied_constructor_produces_correct_variant() {
    let err = RbacServiceError::authorization_denied("no permission");
    assert!(matches!(err, RbacServiceError::AuthorizationDenied { .. }));
    assert!(err.to_string().contains("no permission"));
}

#[test]
fn conflict_constructor_produces_correct_variant() {
    let err = RbacServiceError::conflict("duplicate");
    assert!(matches!(err, RbacServiceError::Conflict { .. }));
    assert!(err.to_string().contains("duplicate"));
}

#[test]
fn dependency_unavailable_constructor_names_the_dependency() {
    let err = RbacServiceError::dependency_unavailable("TenantResolverClient");
    assert!(matches!(
        err,
        RbacServiceError::DependencyUnavailable { .. }
    ));
    assert!(err.to_string().contains("TenantResolverClient"));
}

#[test]
fn internal_constructor_produces_internal_variant() {
    let err = RbacServiceError::internal("oops");
    assert!(matches!(err, RbacServiceError::Internal { .. }));
    assert!(err.to_string().contains("oops"));
}

#[test]
fn role_assignment_duplicate_constructor_carries_all_fields() {
    use crate::role_assignment::PrincipalType;
    let id = Uuid::nil();
    let err = RbacServiceError::role_assignment_duplicate(
        id,
        PrincipalType::User,
        "alice",
        "/tenants/t1",
    );
    assert!(
        matches!(
            &err,
            RbacServiceError::RoleAssignmentDuplicate {
                role_definition_id,
                principal_type,
                principal_id,
                scope,
            } if *role_definition_id == id
                && *principal_type == PrincipalType::User
                && principal_id == "alice"
                && scope == "/tenants/t1"
        ),
        "expected RoleAssignmentDuplicate, got {err:?}"
    );
}

#[test]
fn scope_not_within_assignable_scopes_constructor_carries_both_fields() {
    let err = RbacServiceError::scope_not_within_assignable_scopes(
        "/tenants/t1",
        vec!["/tenants/t2".to_owned()],
    );
    assert!(
        matches!(
            &err,
            RbacServiceError::ScopeNotWithinAssignableScopes {
                scope,
                assignable_scopes,
            } if scope == "/tenants/t1"
                && assignable_scopes.len() == 1
                && assignable_scopes[0] == "/tenants/t2"
        ),
        "expected ScopeNotWithinAssignableScopes, got {err:?}"
    );
}

#[test]
fn group_principal_not_found_constructor_is_404_shaped() {
    let id = Uuid::nil();
    let err = RbacServiceError::group_principal_not_found(id);
    assert!(matches!(
        err,
        RbacServiceError::GroupPrincipalNotFound { .. }
    ));
    assert!(err.to_string().contains(&id.to_string()));
}

#[test]
fn group_principal_root_scope_forbidden_is_unit_variant() {
    let err = RbacServiceError::group_principal_root_scope_forbidden();
    assert!(matches!(
        err,
        RbacServiceError::GroupPrincipalRootScopeForbidden
    ));
    assert!(
        err.to_string()
            .contains("MUST NOT be assigned at root scope")
    );
}

#[test]
fn group_principal_tenant_mismatch_carries_both_tenant_ids() {
    let group_t = uuid::uuid!("11111111-1111-1111-1111-111111111111");
    let scope_t = uuid::uuid!("22222222-2222-2222-2222-222222222222");
    let err = RbacServiceError::group_principal_tenant_mismatch(group_t, scope_t);
    assert!(
        matches!(
            err,
            RbacServiceError::GroupPrincipalTenantMismatch {
                group_tenant_id,
                scope_tenant_id,
            } if group_tenant_id == group_t && scope_tenant_id == scope_t
        ),
        "expected GroupPrincipalTenantMismatch"
    );
}

#[test]
fn invalid_principal_type_constructor_carries_offending_value() {
    let err = RbacServiceError::invalid_principal_type("Robot");
    assert!(
        matches!(
            &err,
            RbacServiceError::InvalidPrincipalType { value } if value == "Robot"
        ),
        "expected InvalidPrincipalType, got {err:?}"
    );
}

#[test]
fn invalid_stored_scope_constructor_carries_offending_scope() {
    let err = RbacServiceError::invalid_stored_scope("/not/a/scope");
    assert!(
        matches!(
            &err,
            RbacServiceError::InvalidStoredScope { scope } if scope == "/not/a/scope"
        ),
        "expected InvalidStoredScope, got {err:?}"
    );
}

#[test]
fn invalid_stored_scope_display_includes_bad_scope_verbatim() {
    let err = RbacServiceError::invalid_stored_scope("/tenants/T1/resourceGroups/");
    let rendered = err.to_string();
    assert!(
        rendered.contains("/tenants/T1/resourceGroups/"),
        "expected display to contain bad scope verbatim, got: {rendered}"
    );
}

#[test]
fn every_error_variant_renders_non_empty_display() {
    // Adding a new variant without a `#[error("...")]` attribute fails this test.
    let id = Uuid::nil();
    let variants = [
        RbacServiceError::RoleDefinitionNotFound { id },
        RbacServiceError::RoleAssignmentNotFound { id },
        RbacServiceError::Validation {
            message: "v".to_owned(),
        },
        RbacServiceError::AuthorizationDenied {
            message: "a".to_owned(),
        },
        RbacServiceError::Conflict {
            message: "c".to_owned(),
        },
        RbacServiceError::DependencyUnavailable {
            dependency: "d".to_owned(),
        },
        RbacServiceError::Internal {
            message: "i".to_owned(),
        },
        RbacServiceError::RoleDefinitionNameTaken {
            name: "Auditor".to_owned(),
            owner_tenant_id: Some(id),
        },
        RbacServiceError::RoleDefinitionNameReservedByBuiltin {
            name: "Owner".to_owned(),
        },
        RbacServiceError::RoleDefinitionAssignmentsExist {
            role_definition_id: id,
        },
        RbacServiceError::BuiltInRoleNotModifiable {
            role_definition_id: id,
        },
        RbacServiceError::InvalidPermissionRule {
            detail: "permissions[0].operation".to_owned(),
        },
        RbacServiceError::ImmutableFieldRejected {
            field: "is_built_in".to_owned(),
        },
        RbacServiceError::OwnerTenantMismatch,
        RbacServiceError::OwnerTenantRequired,
        RbacServiceError::OptimisticConcurrencyMissing,
        RbacServiceError::OptimisticConcurrencyStale {
            current_etag: "1970-01-01T00:00:00.000000Z:00000000-0000-0000-0000-000000000000"
                .to_owned(),
        },
        RbacServiceError::RoleAssignmentDuplicate {
            role_definition_id: id,
            principal_type: crate::role_assignment::PrincipalType::User,
            principal_id: "alice".to_owned(),
            scope: "/tenants/t1".to_owned(),
        },
        RbacServiceError::ScopeNotWithinAssignableScopes {
            scope: "/tenants/t1".to_owned(),
            assignable_scopes: vec!["/tenants/t2".to_owned()],
        },
        RbacServiceError::GroupPrincipalNotFound { principal_id: id },
        RbacServiceError::GroupPrincipalRootScopeForbidden,
        RbacServiceError::GroupPrincipalTenantMismatch {
            group_tenant_id: id,
            scope_tenant_id: id,
        },
        RbacServiceError::InvalidPrincipalType {
            value: "Robot".to_owned(),
        },
        RbacServiceError::InvalidStoredScope {
            scope: "/not/a/scope".to_owned(),
        },
        RbacServiceError::ValidationFailed {
            errors: vec![FieldError::new("name", "must not be empty", "empty")],
        },
    ];
    for variant in variants {
        assert!(
            !variant.to_string().is_empty(),
            "variant {variant:?} renders to an empty string"
        );
    }
}

#[test]
fn field_error_round_trips_through_serde() {
    let original = FieldError::new("permissions[0].operation", "must not be empty", "empty");
    let _back = round_trip(&original);
}

#[test]
fn validation_failed_constructor_preserves_errors_order() {
    let errors = vec![
        FieldError::new("a", "x", "empty"),
        FieldError::new("b", "y", "format"),
        FieldError::new("c", "z", "range"),
    ];
    let err = RbacServiceError::validation_failed(errors.clone());
    assert!(
        matches!(&err, RbacServiceError::ValidationFailed { errors: out } if out == &errors),
        "expected ValidationFailed with preserved order, got {err:?}"
    );
}

#[test]
fn validation_failed_display_carries_count() {
    let err = RbacServiceError::validation_failed(vec![
        FieldError::new("a", "x", "empty"),
        FieldError::new("b", "y", "format"),
    ]);
    let rendered = err.to_string();
    assert!(
        rendered.contains('2'),
        "display must mention the count, got: {rendered}"
    );
}

#[test]
fn validation_failed_caps_oversized_errors_array_with_sentinel() {
    let oversized: Vec<FieldError> = (0..MAX_FIELD_ERRORS + 50)
        .map(|i| FieldError::new(format!("f{i}"), format!("m{i}"), "x"))
        .collect();
    let err = RbacServiceError::validation_failed(oversized);

    assert!(
        matches!(&err, RbacServiceError::ValidationFailed { errors } if errors.len() == MAX_FIELD_ERRORS),
        "errors array must be capped at MAX_FIELD_ERRORS, got {err:?}"
    );
    let RbacServiceError::ValidationFailed { errors } = err else {
        unreachable!("matches! check above guarantees the variant");
    };
    let sentinel = errors.last().expect("non-empty after cap");
    assert_eq!(sentinel.code, TRUNCATION_SENTINEL_CODE);
    assert_eq!(sentinel.field, "$truncated");
    assert!(
        sentinel.message.contains("51"),
        "sentinel message must name the suppressed count, got: {}",
        sentinel.message
    );
}

#[test]
fn validation_failed_passes_through_when_at_cap_exactly() {
    let at_cap: Vec<FieldError> = (0..MAX_FIELD_ERRORS)
        .map(|i| FieldError::new(format!("f{i}"), "m", "x"))
        .collect();
    let err = RbacServiceError::validation_failed(at_cap.clone());

    assert!(
        matches!(&err, RbacServiceError::ValidationFailed { errors } if errors == &at_cap),
        "no sentinel should be appended at cap, got {err:?}"
    );
}

#[test]
// One exhaustive `assert!(matches!(..))` per constructor: the arm count is
// what makes the table complete, and splitting it would only hide which
// constructor is missing. Complexity is 21 against a cap of 20.
#[allow(clippy::cognitive_complexity)]
fn new_role_definition_crud_error_constructors_produce_correct_variants() {
    let id = Uuid::nil();

    assert!(matches!(
        RbacServiceError::role_definition_name_taken("Auditor", Some(id)),
        RbacServiceError::RoleDefinitionNameTaken { .. }
    ));
    assert!(matches!(
        RbacServiceError::role_definition_name_reserved_by_builtin("Owner"),
        RbacServiceError::RoleDefinitionNameReservedByBuiltin { .. }
    ));
    assert!(matches!(
        RbacServiceError::role_definition_assignments_exist(id),
        RbacServiceError::RoleDefinitionAssignmentsExist { .. }
    ));
    assert!(matches!(
        RbacServiceError::built_in_role_not_modifiable(id),
        RbacServiceError::BuiltInRoleNotModifiable { .. }
    ));
    assert!(matches!(
        RbacServiceError::invalid_permission_rule("permissions[0]"),
        RbacServiceError::InvalidPermissionRule { .. }
    ));
    assert!(matches!(
        RbacServiceError::immutable_field_rejected("is_built_in"),
        RbacServiceError::ImmutableFieldRejected { .. }
    ));
    assert!(matches!(
        RbacServiceError::owner_tenant_mismatch(),
        RbacServiceError::OwnerTenantMismatch
    ));
    assert!(matches!(
        RbacServiceError::owner_tenant_required(),
        RbacServiceError::OwnerTenantRequired
    ));
    assert!(matches!(
        RbacServiceError::optimistic_concurrency_missing(),
        RbacServiceError::OptimisticConcurrencyMissing
    ));
    assert!(matches!(
        RbacServiceError::optimistic_concurrency_stale("etag"),
        RbacServiceError::OptimisticConcurrencyStale { .. }
    ));
}
