//! Unit tests for the parent module, kept out of line: an inline
//! `#[cfg(test)]` block of this size is a lint error in this workspace.

use super::*;
use sea_orm::Iterable;

/// Drift guard: every `Column` variant maps to a column declared by the
/// `role_assignments` migration chain
/// (`m20260521_000002_create_role_assignments_table` plus
/// `m20260824_000003_add_role_assignment_author_identity`).
fn assignment_row(scope: &str, scope_depth: i32, tenant_id: Option<Uuid>) -> Model {
    let now = Utc::now();
    Model {
        id: Uuid::now_v7(),
        role_definition_id: Uuid::now_v7(),
        principal_id: "subject".to_owned(),
        principal_type: "User".to_owned(),
        scope: scope.to_owned(),
        scope_depth,
        tenant_id,
        created_at: now,
        updated_at: now,
        created_by: "test".to_owned(),
        created_by_type: None,
        created_by_tenant_id: None,
    }
}

#[test]
fn entity_columns_match_migration() {
    let expected: std::collections::HashSet<&str> = [
        "id",
        "role_definition_id",
        "principal_id",
        "principal_type",
        "scope",
        "scope_depth",
        "tenant_id",
        "created_at",
        "updated_at",
        "created_by",
        "created_by_type",
        "created_by_tenant_id",
    ]
    .into_iter()
    .collect();

    let actual: std::collections::HashSet<String> = Column::iter().map(|c| c.to_string()).collect();

    for col in &actual {
        assert!(
            expected.contains(col.as_str()),
            "Column '{col}' in entity is not present in the RBAC \
             role_assignments migration chain - either a migration or the \
             entity needs updating",
        );
    }
    for col in &expected {
        assert!(
            actual.contains(*col),
            "Column '{col}' from the RBAC role_assignments migration chain is \
             missing from entity - add it to Model or leave a documented rationale",
        );
    }
}

#[test]
fn entity_to_model_accepts_scope_with_consistent_query_projection() {
    let tenant_id = Uuid::new_v4();
    let row = assignment_row(&format!("/tenants/{tenant_id}"), 2, Some(tenant_id));

    let mapped = entity_to_model(row).expect("consistent scope projection must map");

    assert_eq!(mapped.scope, rbac_sdk::models::Scope::tenant(tenant_id));
}

#[test]
fn entity_to_model_rejects_root_scope_disguised_as_tenant_candidate() {
    let row = assignment_row("/", 1, Some(Uuid::new_v4()));

    let error =
        entity_to_model(row).expect_err("a root scope with tenant query metadata must fail closed");

    assert!(matches!(
        error,
        RoleAssignmentMappingError::InconsistentScopeProjection { .. }
    ));
}

#[test]
fn entity_to_model_rejects_inconsistent_scope_depth() {
    let tenant_id = Uuid::new_v4();
    let row = assignment_row(&format!("/tenants/{tenant_id}"), 1, Some(tenant_id));

    let error = entity_to_model(row)
        .expect_err("scope depth must be derived from the canonical scope path");

    assert!(matches!(
        error,
        RoleAssignmentMappingError::InconsistentScopeProjection { .. }
    ));
}
