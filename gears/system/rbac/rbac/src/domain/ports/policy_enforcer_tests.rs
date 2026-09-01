//! Unit tests for [`super::MockPolicyEnforcer`].

use super::{
    AuthorizationError, Decision, MatchPred, MockPolicyEnforcer, PolicyEnforcer, ReadableScopes,
    ReadableScopesPred,
};
// `project_readable_scopes` lives in `permission_evaluator.rs`; DE1101
// forbids re-exports of `#[cfg(test)]` items, so this test reaches in
// directly.
use crate::domain::permission_evaluator::project_readable_scopes;
use rbac_sdk::models::{PermissionRule, PrincipalType, SubjectRole};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Helper that keeps the `SecurityContext` plumbing out of test bodies.
/// The mock ignores `ctx`; only production code forwards it.
fn test_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(uuid::uuid!("00000000-0000-0000-0000-00000000aaaa"))
        .subject_tenant_id(uuid::uuid!("00000000-0000-0000-0000-00000000bbbb"))
        .build()
        .expect("test SecurityContext must build")
}

#[tokio::test]
async fn allow_all_grants_every_request() {
    let enforcer = MockPolicyEnforcer::allow_all();
    enforcer
        .enforce(
            &test_ctx(),
            "subject-1",
            PrincipalType::User,
            "write",
            "gts.cf.core.rbac.role_definition.v1~",
            &rbac_sdk::models::Scope::tenant(uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")),
        )
        .await
        .expect("allow-all MUST grant");
}

#[tokio::test]
async fn deny_all_denies_every_request() {
    let enforcer = MockPolicyEnforcer::deny_all();
    let result = enforcer
        .enforce(
            &test_ctx(),
            "subject-1",
            PrincipalType::User,
            "write",
            "gts.cf.core.rbac.role_definition.v1~",
            &rbac_sdk::models::Scope::tenant(uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")),
        )
        .await;
    assert_eq!(result, Err(AuthorizationError::Denied));
}

#[tokio::test]
async fn match_table_first_match_wins() {
    let enforcer = MockPolicyEnforcer::match_table(vec![
        (
            MatchPred {
                operation: Some("read".to_owned()),
                ..Default::default()
            },
            Decision::Allow,
        ),
        (
            MatchPred {
                operation: Some("write".to_owned()),
                ..Default::default()
            },
            Decision::Deny,
        ),
    ]);

    enforcer
        .enforce(
            &test_ctx(),
            "s",
            PrincipalType::User,
            "read",
            "t",
            &rbac_sdk::models::Scope::Root,
        )
        .await
        .expect("read allowed");
    assert_eq!(
        enforcer
            .enforce(
                &test_ctx(),
                "s",
                PrincipalType::User,
                "write",
                "t",
                &rbac_sdk::models::Scope::Root
            )
            .await,
        Err(AuthorizationError::Denied)
    );
}

#[tokio::test]
async fn match_table_default_is_deny_when_no_predicate_matches() {
    let enforcer = MockPolicyEnforcer::match_table(vec![(
        MatchPred {
            operation: Some("read".to_owned()),
            ..Default::default()
        },
        Decision::Allow,
    )]);

    let result = enforcer
        .enforce(
            &test_ctx(),
            "subject-1",
            PrincipalType::User,
            "delete",
            "target-1",
            &rbac_sdk::models::Scope::Root,
        )
        .await;
    assert_eq!(
        result,
        Err(AuthorizationError::Denied),
        "unknown predicate MUST default to deny"
    );
}

#[tokio::test]
async fn match_table_combines_multiple_pred_fields_with_and() {
    let enforcer = MockPolicyEnforcer::match_table(vec![(
        MatchPred {
            subject_id: Some("alice".to_owned()),
            operation: Some("write".to_owned()),
            ..Default::default()
        },
        Decision::Allow,
    )]);

    enforcer
        .enforce(
            &test_ctx(),
            "alice",
            PrincipalType::User,
            "write",
            "t",
            &rbac_sdk::models::Scope::Root,
        )
        .await
        .expect("both fields match");

    let result = enforcer
        .enforce(
            &test_ctx(),
            "bob",
            PrincipalType::User,
            "write",
            "t",
            &rbac_sdk::models::Scope::Root,
        )
        .await;
    assert_eq!(result, Err(AuthorizationError::Denied));

    let result = enforcer
        .enforce(
            &test_ctx(),
            "alice",
            PrincipalType::User,
            "read",
            "t",
            &rbac_sdk::models::Scope::Root,
        )
        .await;
    assert_eq!(result, Err(AuthorizationError::Denied));
}

#[tokio::test]
async fn calls_are_recorded() {
    let enforcer = MockPolicyEnforcer::allow_all();
    enforcer
        .enforce(
            &test_ctx(),
            "subject-1",
            PrincipalType::User,
            "write",
            "target-1",
            &rbac_sdk::models::Scope::tenant(uuid::uuid!("11111111-1111-1111-1111-111111111111")),
        )
        .await
        .expect("allowed");
    enforcer
        .enforce(
            &test_ctx(),
            "subject-2",
            PrincipalType::ServicePrincipal,
            "read",
            "target-2",
            &rbac_sdk::models::Scope::tenant(uuid::uuid!("22222222-2222-2222-2222-222222222222")),
        )
        .await
        .expect("allowed");

    let calls = enforcer.recorded_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0],
        (
            "subject-1".to_owned(),
            PrincipalType::User,
            "write".to_owned(),
            "target-1".to_owned(),
            rbac_sdk::models::Scope::tenant(uuid::uuid!("11111111-1111-1111-1111-111111111111")),
        )
    );
    assert_eq!(
        calls[1],
        (
            "subject-2".to_owned(),
            PrincipalType::ServicePrincipal,
            "read".to_owned(),
            "target-2".to_owned(),
            rbac_sdk::models::Scope::tenant(uuid::uuid!("22222222-2222-2222-2222-222222222222")),
        )
    );
}

// ===========================================================================
// readable_scopes — MockPolicyEnforcer matrix
// ===========================================================================

fn role_assignment_read() -> PermissionRule {
    PermissionRule::new("read", "gts.cf.core.rbac.role_assignment.v1~")
}

/// Build a [`SubjectRole`] with allow-only rules. Use [`role_with_deny`]
/// for tests that need deny rules.
fn role(scope: &str, rules: Vec<PermissionRule>) -> SubjectRole {
    role_with_deny(scope, rules, Vec::new())
}

fn role_with_deny(
    scope: &str,
    permissions: Vec<PermissionRule>,
    not_permissions: Vec<PermissionRule>,
) -> SubjectRole {
    SubjectRole::new(
        Uuid::now_v7(),
        Uuid::now_v7(),
        "role",
        permissions,
        not_permissions,
        rbac_sdk::models::Scope::parse(scope).expect("test scope must be valid path"),
        false,
        "subject",
        PrincipalType::User,
    )
}

#[tokio::test]
async fn mock_readable_scopes_default_is_none() {
    let enforcer = MockPolicyEnforcer::allow_all();
    let outcome = enforcer
        .readable_scopes(
            &test_ctx(),
            "subject-1",
            PrincipalType::User,
            "gts.cf.core.rbac.role_assignment.v1~",
            &rbac_sdk::models::Scope::Root,
        )
        .await
        .expect("infallible");
    assert_eq!(outcome, ReadableScopes::None);
}

#[tokio::test]
async fn mock_readable_scopes_matches_subject_and_target() {
    let enforcer = MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred {
            subject_id: Some("alice".to_owned()),
            target_type: Some("gts.cf.core.rbac.role_assignment.v1~".to_owned()),
            ..Default::default()
        },
        ReadableScopes::Subtrees(vec![
            "/tenants/11111111-1111-1111-1111-111111111111".to_owned(),
        ]),
    )]);

    let outcome = enforcer
        .readable_scopes(
            &test_ctx(),
            "alice",
            PrincipalType::User,
            "gts.cf.core.rbac.role_assignment.v1~",
            &rbac_sdk::models::Scope::Root,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        ReadableScopes::Subtrees(vec![
            "/tenants/11111111-1111-1111-1111-111111111111".to_owned()
        ])
    );

    // Different subject falls through to default `None`.
    let outcome = enforcer
        .readable_scopes(
            &test_ctx(),
            "bob",
            PrincipalType::User,
            "gts.cf.core.rbac.role_assignment.v1~",
            &rbac_sdk::models::Scope::Root,
        )
        .await
        .unwrap();
    assert_eq!(outcome, ReadableScopes::None);
}

#[tokio::test]
async fn mock_readable_scopes_unrestricted_shortcut() {
    let enforcer = MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred {
            subject_id: Some("root-admin".to_owned()),
            ..Default::default()
        },
        ReadableScopes::Unrestricted,
    )]);
    let outcome = enforcer
        .readable_scopes(
            &test_ctx(),
            "root-admin",
            PrincipalType::User,
            "any.target.v1~",
            &rbac_sdk::models::Scope::Root,
        )
        .await
        .unwrap();
    assert_eq!(outcome, ReadableScopes::Unrestricted);
}

// ===========================================================================
// project_readable_scopes — production projector
// ===========================================================================

#[test]
fn project_no_roles_yields_none() {
    let outcome = project_readable_scopes(&[], "gts.cf.core.rbac.role_assignment.v1~");
    assert_eq!(outcome, ReadableScopes::None);
}

#[test]
fn project_role_with_no_matching_read_yields_none() {
    let r = role(
        "/tenants/11111111-1111-1111-1111-111111111111",
        vec![PermissionRule::new(
            "write",
            "gts.cf.core.rbac.role_assignment.v1~",
        )],
    );
    let outcome = project_readable_scopes(&[r], "gts.cf.core.rbac.role_assignment.v1~");
    assert_eq!(outcome, ReadableScopes::None);
}

#[test]
fn project_role_granting_read_yields_subtree() {
    let r = role(
        "/tenants/11111111-1111-1111-1111-111111111111",
        vec![role_assignment_read()],
    );
    let outcome = project_readable_scopes(&[r], "gts.cf.core.rbac.role_assignment.v1~");
    assert_eq!(
        outcome,
        ReadableScopes::Subtrees(vec![
            "/tenants/11111111-1111-1111-1111-111111111111".to_owned()
        ])
    );
}

#[test]
fn project_root_scoped_role_short_circuits_to_unrestricted() {
    let r = role("/", vec![role_assignment_read()]);
    let outcome = project_readable_scopes(&[r], "gts.cf.core.rbac.role_assignment.v1~");
    assert_eq!(outcome, ReadableScopes::Unrestricted);
}

#[test]
fn project_unrestricted_wins_over_specific_subtrees() {
    let r1 = role(
        "/tenants/11111111-1111-1111-1111-111111111111",
        vec![role_assignment_read()],
    );
    let r2 = role("/", vec![role_assignment_read()]);
    let outcome = project_readable_scopes(&[r1, r2], "gts.cf.core.rbac.role_assignment.v1~");
    assert_eq!(outcome, ReadableScopes::Unrestricted);
}

#[test]
fn project_not_permissions_subtract_grants() {
    // A matching `not_permissions` entry masks the grant: the deny pass
    // short-circuits the role.
    let r = role_with_deny(
        "/tenants/11111111-1111-1111-1111-111111111111",
        vec![role_assignment_read()],
        vec![rbac_sdk::models::PermissionRule::new(
            "read",
            "gts.cf.core.rbac.role_assignment.v1~",
        )],
    );
    let outcome = project_readable_scopes(&[r], "gts.cf.core.rbac.role_assignment.v1~");
    assert_eq!(outcome, ReadableScopes::None);
}

#[test]
fn project_deduplicates_repeated_scopes() {
    let r1 = role(
        "/tenants/11111111-1111-1111-1111-111111111111",
        vec![role_assignment_read()],
    );
    let r2 = role(
        "/tenants/11111111-1111-1111-1111-111111111111",
        vec![role_assignment_read()],
    );
    let r3 = role(
        "/tenants/22222222-2222-2222-2222-222222222222",
        vec![role_assignment_read()],
    );
    let outcome = project_readable_scopes(&[r1, r2, r3], "gts.cf.core.rbac.role_assignment.v1~");
    let ReadableScopes::Subtrees(scopes) = outcome else {
        unreachable!("expected Subtrees, got {outcome:?}")
    };
    assert_eq!(scopes.len(), 2);
    assert!(scopes.contains(&"/tenants/11111111-1111-1111-1111-111111111111".to_owned()));
    assert!(scopes.contains(&"/tenants/22222222-2222-2222-2222-222222222222".to_owned()));
}

#[test]
fn project_ignores_roles_granting_other_target_types() {
    let r = role(
        "/tenants/11111111-1111-1111-1111-111111111111",
        vec![PermissionRule::new(
            "read",
            "gts.cf.resources.compute.vm.v1~",
        )],
    );
    let outcome = project_readable_scopes(&[r], "gts.cf.core.rbac.role_assignment.v1~");
    assert_eq!(outcome, ReadableScopes::None);
}

#[test]
fn project_recognises_wildcard_operation() {
    let r = role(
        "/tenants/11111111-1111-1111-1111-111111111111",
        vec![PermissionRule::new(
            "*",
            "gts.cf.core.rbac.role_assignment.v1~",
        )],
    );
    let outcome = project_readable_scopes(&[r], "gts.cf.core.rbac.role_assignment.v1~");
    assert_eq!(
        outcome,
        ReadableScopes::Subtrees(vec![
            "/tenants/11111111-1111-1111-1111-111111111111".to_owned()
        ])
    );
}

// Production enforcer behaviour is exercised through the `project_*`
// tests above (which call `project_readable_scopes` directly) and the
// integration suite (which surfaces `list_role_assignments` failures if
// `include_group_roles = true` regresses).
