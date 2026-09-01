//! Pure-CPU classifier tests for [`determine_scope_type`],
//! [`aggregate_scope_types`], and [`is_scope_inherited`]. These are pure
//! functions of `String` / `Vec<PermissionScopeType>` / `Uuid` and perform no
//! I/O, so they live outside the Postgres integration suite in
//! `tests/postgres_permission_evaluator.rs` — whose tests are `#[ignore]`d and
//! pull in testcontainers, meaning classifier regressions would otherwise only
//! be caught by a full `--ignored` run.
//!
//! Run with: `cargo test -p cf-gears-rbac --test permission_evaluator_classifier`

#![cfg(test)]
#![allow(clippy::expect_used, clippy::doc_markdown, clippy::panic)]

use rbac::domain::permission_evaluator::{
    aggregate_scope_types, determine_scope_type, is_scope_inherited,
};
use rbac_sdk::models::Scope;
use rbac_sdk::subject_role::PermissionScopeType;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// determine_scope_type
// ---------------------------------------------------------------------------

#[test]
fn root_scope_classifies_as_global() {
    let result = determine_scope_type("/").expect("classifier rejected '/'");
    assert!(
        matches!(result, PermissionScopeType::Global),
        "expected Global, got {result:?}"
    );
}

#[test]
fn tenant_scope_classifies_as_tenant_subtree() {
    let t = Uuid::new_v4();
    let scope = format!("/tenants/{t}");
    let result = determine_scope_type(&scope).expect("classifier rejected tenant scope");
    assert!(
        matches!(
            result,
            PermissionScopeType::TenantSubtree { root_tenant_id } if root_tenant_id == t
        ),
        "expected TenantSubtree {{ {t} }}, got {result:?}"
    );
}

#[test]
fn rg_scope_classifies_as_group_subtree() {
    let t = Uuid::new_v4();
    let rg = Uuid::new_v4();
    let scope = format!("/tenants/{t}/resourceGroups/{rg}");
    let result = determine_scope_type(&scope).expect("classifier rejected RG scope");
    assert!(
        matches!(
            &result,
            PermissionScopeType::GroupSubtree { root_group_ids } if root_group_ids == &vec![rg]
        ),
        "expected GroupSubtree {{ [{rg}] }}, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// aggregate_scope_types
// ---------------------------------------------------------------------------

#[test]
fn single_grant_returns_its_scope_type_directly() {
    let t = Uuid::new_v4();
    let result = aggregate_scope_types(&[PermissionScopeType::TenantSubtree { root_tenant_id: t }])
        .expect("aggregate on non-empty input");
    assert!(matches!(
        result,
        PermissionScopeType::TenantSubtree { root_tenant_id } if root_tenant_id == t
    ));
}

#[test]
fn same_type_pass_through_returns_that_type() {
    let t = Uuid::new_v4();
    let inputs = vec![
        PermissionScopeType::TenantSubtree { root_tenant_id: t },
        PermissionScopeType::TenantSubtree { root_tenant_id: t },
        PermissionScopeType::TenantSubtree { root_tenant_id: t },
    ];
    let result = aggregate_scope_types(&inputs).expect("aggregate on non-empty input");
    assert!(
        matches!(
            result,
            PermissionScopeType::TenantSubtree { root_tenant_id } if root_tenant_id == t
        ),
        "expected TenantSubtree, not Combined"
    );
}

#[test]
fn two_group_subtrees_under_same_tenant_merge() {
    let rg1 = Uuid::new_v4();
    let rg2 = Uuid::new_v4();
    let inputs = vec![
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![rg1],
        },
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![rg2],
        },
    ];
    let result = aggregate_scope_types(&inputs).expect("aggregate on non-empty input");
    match result {
        PermissionScopeType::GroupSubtree { root_group_ids } => {
            assert_eq!(root_group_ids.len(), 2);
            assert!(root_group_ids.contains(&rg1));
            assert!(root_group_ids.contains(&rg2));
        }
        other => panic!("expected merged GroupSubtree, got {other:?}"),
    }
}

#[test]
fn tenant_subtree_plus_group_subtree_combines() {
    let t = Uuid::new_v4();
    let rg = Uuid::new_v4();
    let inputs = vec![
        PermissionScopeType::TenantSubtree { root_tenant_id: t },
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![rg],
        },
    ];
    let result = aggregate_scope_types(&inputs).expect("aggregate on non-empty input");
    match result {
        PermissionScopeType::Combined { scopes } => {
            assert_eq!(scopes.len(), 2);
            assert!(matches!(
                &scopes[0],
                PermissionScopeType::TenantSubtree { root_tenant_id } if root_tenant_id == &t
            ));
            assert!(matches!(
                &scopes[1],
                PermissionScopeType::GroupSubtree { root_group_ids } if root_group_ids == &vec![rg]
            ));
        }
        other => panic!("expected Combined, got {other:?}"),
    }
}

#[test]
fn merge_before_combine_three_grants() {
    let t = Uuid::new_v4();
    let rg1 = Uuid::new_v4();
    let rg2 = Uuid::new_v4();
    let inputs = vec![
        PermissionScopeType::TenantSubtree { root_tenant_id: t },
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![rg1],
        },
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![rg2],
        },
    ];
    let result = aggregate_scope_types(&inputs).expect("aggregate on non-empty input");
    match result {
        PermissionScopeType::Combined { scopes } => {
            assert_eq!(
                scopes.len(),
                2,
                "expected GroupSubtree entries merged into one before Combined wrapping"
            );
            assert!(matches!(
                &scopes[0],
                PermissionScopeType::TenantSubtree { root_tenant_id } if root_tenant_id == &t
            ));
            match &scopes[1] {
                PermissionScopeType::GroupSubtree { root_group_ids } => {
                    assert_eq!(root_group_ids.len(), 2);
                    assert!(root_group_ids.contains(&rg1));
                    assert!(root_group_ids.contains(&rg2));
                }
                other => panic!("expected merged GroupSubtree, got {other:?}"),
            }
        }
        other => panic!("expected Combined, got {other:?}"),
    }
}

#[test]
fn duplicate_rg_ids_are_deduplicated_in_merge() {
    let rg = Uuid::new_v4();
    let inputs = vec![
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![rg],
        },
        PermissionScopeType::GroupSubtree {
            root_group_ids: vec![rg],
        },
    ];
    let result = aggregate_scope_types(&inputs).expect("aggregate on non-empty input");
    match result {
        PermissionScopeType::GroupSubtree { root_group_ids } => {
            assert_eq!(root_group_ids, vec![rg], "duplicates must be removed");
        }
        other => panic!("expected GroupSubtree, got {other:?}"),
    }
}

#[test]
fn global_pass_through() {
    let result = aggregate_scope_types(&[PermissionScopeType::Global, PermissionScopeType::Global])
        .expect("aggregate on non-empty input");
    assert!(matches!(result, PermissionScopeType::Global));
}

/// An empty contributing-scope slice is a caller-invariant
/// violation. `aggregate_scope_types` returns `None` (fail-closed) rather
/// than panicking or defaulting to `Global`, so the evaluator surfaces a
/// 500 `Internal` instead of aborting the worker or widening permissions.
#[test]
fn aggregate_scope_types_returns_none_on_empty_input() {
    assert!(
        aggregate_scope_types(&[]).is_none(),
        "empty input MUST yield None (fail-closed), never a default scope type"
    );
}

// ---------------------------------------------------------------------------
// is_scope_inherited
// ---------------------------------------------------------------------------

#[test]
fn is_inherited_true_for_root() {
    assert!(is_scope_inherited(&Scope::root(), Uuid::new_v4()));
}

#[test]
fn is_inherited_true_for_ancestor_tenant() {
    let ctx = Uuid::new_v4();
    let ancestor = Uuid::new_v4();
    assert!(is_scope_inherited(&Scope::tenant(ancestor), ctx));
}

#[test]
fn is_inherited_false_for_context_tenant() {
    let ctx = Uuid::new_v4();
    assert!(!is_scope_inherited(&Scope::tenant(ctx), ctx));
}

#[test]
fn is_inherited_false_for_rg_under_context_tenant() {
    let ctx = Uuid::new_v4();
    let rg = Uuid::new_v4();
    assert!(!is_scope_inherited(&Scope::resource_group(ctx, rg), ctx));
}

/// An RG under a *different* (ancestor) tenant is inherited. The decision is
/// made by `tenant_id != context` rather than by a "starts with /tenants/"
/// string prefix, which keeps the sibling-prefix bug class (`/tenants/T1` vs
/// `/tenants/T10`) out.
#[test]
fn is_inherited_true_for_rg_under_ancestor_tenant() {
    let ctx = Uuid::new_v4();
    let ancestor = Uuid::new_v4();
    let rg = Uuid::new_v4();
    assert!(is_scope_inherited(
        &Scope::resource_group(ancestor, rg),
        ctx
    ));
}
