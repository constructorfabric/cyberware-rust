//! Postgres-backed integration tests for [`PermissionEvaluator`] — the
//! evaluator traverses real `role_definitions` / `role_assignments` rows
//! against an ephemeral PostgreSQL container.

#![cfg(test)]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::doc_markdown
)]

mod common;

use std::sync::Arc;

use anyhow::Result;
use rbac::domain::model::RoleAssignmentModel;
use rbac_sdk::models::{
    DenyReason, PermissionResult, PermissionRule, PermissionScopeType, PrincipalType, Scope,
};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

use rbac::domain::permission_evaluator::{
    PermissionEvaluator, ScopeClassifyError, determine_scope_type,
};
use rbac::domain::role_assignment_repo::{NewRoleAssignment, RoleAssignmentRepository};
use rbac::domain::role_definition_repo::{NewRoleDefinition, RoleDefinitionRepository};
use rbac::infra::storage::role_assignment_repo;
use rbac::infra::storage::role_definition_repo;

use common::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};

// ---------------------------------------------------------------------------
// Harness — fresh Postgres testcontainer per test.
// ---------------------------------------------------------------------------

struct EvaluatorHarness {
    /// The evaluator's connection source. The repos own none.
    provider: toolkit_db::DBProvider<toolkit_db::DbError>,
    assignment_repo: Arc<role_assignment_repo::RoleAssignmentRepository>,
    definition_repo: Arc<role_definition_repo::RoleDefinitionRepository>,
    _fixture: common::PostgresUnderTest,
}

async fn fresh_harness() -> Result<EvaluatorHarness> {
    let fixture = common::bring_up_migrated_postgres().await?;
    // Two independent `DBProvider`s, matching how `init()` wires production.
    let db_assignments = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let provider_assignments: DBProvider<DbError> = DBProvider::new(db_assignments);
    let db_definitions = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let provider_definitions: DBProvider<DbError> = DBProvider::new(db_definitions);

    // One provider is enough: neither repo owns a connection.
    let _ = provider_definitions;
    Ok(EvaluatorHarness {
        provider: provider_assignments,
        assignment_repo: Arc::new(role_assignment_repo::RoleAssignmentRepository),
        definition_repo: Arc::new(role_definition_repo::RoleDefinitionRepository),
        _fixture: fixture,
    })
}

fn make_evaluator(
    provider: toolkit_db::DBProvider<toolkit_db::DbError>,
    assignment_repo: Arc<role_assignment_repo::RoleAssignmentRepository>,
    definition_repo: Arc<role_definition_repo::RoleDefinitionRepository>,
    tenants: Arc<FakeTenantResolverClient>,
    rg: Arc<FakeRbacRgRead>,
) -> PermissionEvaluator<
    role_assignment_repo::RoleAssignmentRepository,
    role_definition_repo::RoleDefinitionRepository,
> {
    PermissionEvaluator::new(
        provider,
        assignment_repo,
        definition_repo,
        tenants,
        rg,
        Arc::new(rbac::domain::metrics::NoopMetrics),
    )
}

/// Seed a custom role definition via the SeaORM repo.
async fn seed_role_definition(
    conn: &toolkit_db::secure::DbConn<'_>,
    repo: &Arc<role_definition_repo::RoleDefinitionRepository>,
    id: Uuid,
    name: &str,
    permissions: Vec<PermissionRule>,
    not_permissions: Vec<PermissionRule>,
) {
    repo.create(
        conn,
        NewRoleDefinition {
            id,
            name: name.to_owned(),
            description: None,
            permissions,
            not_permissions,
            assignable_scopes: vec![Scope::Root],
            owner_tenant_id: Uuid::new_v4(),
            created_by: "system".to_owned(),
        },
    )
    .await
    .expect("seed_role_definition: SeaORM create must succeed");
}

/// Seed a role assignment via the SeaORM repo. Returns the new row's id.
async fn seed_assignment(
    conn: &toolkit_db::secure::DbConn<'_>,
    repo: &Arc<role_assignment_repo::RoleAssignmentRepository>,
    role_id: Uuid,
    principal_id: &str,
    principal_type: PrincipalType,
    scope: &str,
) -> Uuid {
    let row = repo
        .create(
            conn,
            NewRoleAssignment {
                role_definition_id: role_id,
                principal_id: principal_id.to_owned(),
                principal_type,
                scope: rbac_sdk::models::Scope::parse(scope)
                    .expect("test scope must be a valid path"),
                created_by: "system".to_owned(),
                // The author identity is a display-path concern; these
                // fixtures seed rows directly and record none.
                created_by_type: None,
                created_by_tenant_id: None,
            },
        )
        .await
        .expect("seed_assignment: SeaORM create must succeed");
    row.id
}

// ---------------------------------------------------------------------------
// Pure-CPU classifier / aggregator / is-inherited tests moved to
// `tests/permission_evaluator_classifier.rs` so they run on
// every `cargo test` instead of being gated behind `--ignored` along
// with the testcontainer-bound tests below.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// PermissionEvaluator::get_subject_roles.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_roles_returns_ancestor_and_context_assignments() -> Result<()> {
    let harness = fresh_harness().await?;
    let ctx_tenant = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[ctx_tenant]));
    let rg = Arc::new(FakeRbacRgRead::default());

    let role_id = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_id,
        "Reader",
        vec![PermissionRule::new("read", "gts.cf.test.example.vm.v1~")],
        Vec::new(),
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        "/",
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{ctx_tenant}"),
    )
    .await;

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let roles = evaluator
        .get_subject_roles(&sec, "alice", PrincipalType::User, ctx_tenant, false)
        .await
        .unwrap();
    assert_eq!(roles.len(), 2);
    let scope_paths: Vec<String> = roles.iter().map(|r| r.scope.path()).collect();
    let scopes: Vec<&str> = scope_paths.iter().map(String::as_str).collect();
    assert!(scopes.contains(&"/"));
    assert!(
        scopes
            .iter()
            .any(|s| s == &format!("/tenants/{ctx_tenant}").as_str())
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_roles_includes_rg_assignment_under_context_tenant() -> Result<()> {
    let harness = fresh_harness().await?;
    let ctx_tenant = Uuid::new_v4();
    let rg_id = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[ctx_tenant]));
    let rg = Arc::new(FakeRbacRgRead::default());

    let role_id = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_id,
        "RG-Reader",
        vec![],
        vec![],
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{ctx_tenant}/resourceGroups/{rg_id}"),
    )
    .await;

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let roles = evaluator
        .get_subject_roles(&sec, "alice", PrincipalType::User, ctx_tenant, false)
        .await
        .unwrap();
    assert_eq!(roles.len(), 1);
    assert_eq!(
        roles[0].scope.path(),
        format!("/tenants/{ctx_tenant}/resourceGroups/{rg_id}")
    );
    assert!(!roles[0].is_inherited);
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_roles_excludes_rg_assignment_under_ancestor_tenant() -> Result<()> {
    let harness = fresh_harness().await?;
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let rg_parent = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[parent, child]));
    let rg = Arc::new(FakeRbacRgRead::default());

    let role_id = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_id,
        "AncestorRG",
        vec![],
        vec![],
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{parent}/resourceGroups/{rg_parent}"),
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{child}"),
    )
    .await;

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let roles = evaluator
        .get_subject_roles(&sec, "alice", PrincipalType::User, child, false)
        .await
        .unwrap();
    let scopes: Vec<String> = roles.iter().map(|r| r.scope.path()).collect();
    assert!(
        !scopes.contains(&format!("/tenants/{parent}/resourceGroups/{rg_parent}")),
        "ancestor-tenant RG must be excluded; got {scopes:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_roles_skips_membership_lookup_when_include_group_roles_false() -> Result<()> {
    let harness = fresh_harness().await?;
    let ctx_tenant = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[ctx_tenant]));
    let rg = Arc::new(FakeRbacRgRead::default());
    let counter = Arc::clone(&rg.list_memberships_calls);

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        Arc::clone(&tenants),
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let _ = evaluator
        .get_subject_roles(&sec, "alice", PrincipalType::User, ctx_tenant, false)
        .await
        .unwrap();
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "list_memberships must not be called when include_group_roles=false"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_roles_skips_membership_lookup_for_non_user_principal() -> Result<()> {
    let harness = fresh_harness().await?;
    let ctx_tenant = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[ctx_tenant]));
    let rg = Arc::new(FakeRbacRgRead::default());
    let counter = Arc::clone(&rg.list_memberships_calls);

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let _ = evaluator
        .get_subject_roles(
            &sec,
            "alice",
            PrincipalType::ServicePrincipal,
            ctx_tenant,
            true,
        )
        .await
        .unwrap();
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_roles_multi_page_membership_rolls_into_one_query() -> Result<()> {
    let harness = fresh_harness().await?;
    let ctx_tenant = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[ctx_tenant]));

    let g1 = Uuid::new_v4();
    let g2 = Uuid::new_v4();
    let g3 = Uuid::new_v4();
    let rg = Arc::new(FakeRbacRgRead::default().with_membership_pages(vec![
        vec![g1],
        vec![g2],
        vec![g3],
    ]));
    let counter = Arc::clone(&rg.list_memberships_calls);

    let role_id = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_id,
        "GroupReader",
        vec![],
        vec![],
    )
    .await;
    for gid in &[g1, g2, g3] {
        seed_assignment(
            &harness.provider.conn()?,
            &harness.assignment_repo,
            role_id,
            &gid.to_string(),
            PrincipalType::Group,
            &format!("/tenants/{ctx_tenant}"),
        )
        .await;
    }

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let roles = evaluator
        .get_subject_roles(&sec, "alice", PrincipalType::User, ctx_tenant, true)
        .await
        .unwrap();
    assert_eq!(roles.len(), 3, "expected all three group-held assignments");
    assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_roles_sets_is_inherited_correctly() -> Result<()> {
    let harness = fresh_harness().await?;
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let rg_id = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[parent, child]));
    let rg = Arc::new(FakeRbacRgRead::default());

    let role_id = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_id,
        "Mixed",
        vec![],
        vec![],
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{parent}"),
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{child}"),
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{child}/resourceGroups/{rg_id}"),
    )
    .await;

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let roles = evaluator
        .get_subject_roles(&sec, "alice", PrincipalType::User, child, false)
        .await
        .unwrap();
    for sr in &roles {
        match sr.scope.path().as_str() {
            s if s == format!("/tenants/{parent}") => assert!(sr.is_inherited),
            s if s == format!("/tenants/{child}") => assert!(!sr.is_inherited),
            s if s == format!("/tenants/{child}/resourceGroups/{rg_id}") => {
                assert!(!sr.is_inherited);
            }
            other => panic!("unexpected scope {other}"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// PermissionEvaluator::evaluate_permission.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn evaluate_permission_allowed_with_single_role() -> Result<()> {
    let harness = fresh_harness().await?;
    let ctx_tenant = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[ctx_tenant]));
    let rg = Arc::new(FakeRbacRgRead::default());

    let role_id = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_id,
        "Reader",
        vec![PermissionRule::new("read", "gts.cf.test.example.vm.v1~")],
        Vec::new(),
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{ctx_tenant}"),
    )
    .await;

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let result = evaluator
        .evaluate_permission(
            &sec,
            "alice",
            PrincipalType::User,
            "read",
            "gts.cf.test.example.vm.v1~",
            &Scope::Tenant {
                tenant_id: ctx_tenant,
            },
        )
        .await
        .unwrap();
    match result {
        PermissionResult::Allowed(granted) => {
            assert_eq!(granted.grants.len(), 1);
            assert_eq!(granted.grants[0].role_definition_id, role_id);
            assert!(matches!(
                granted.scope_type,
                PermissionScopeType::TenantSubtree { root_tenant_id } if root_tenant_id == ctx_tenant
            ));
        }
        other => panic!("expected Allowed, got {other:?}"),
    }
    Ok(())
}

/// Read/authorize parity: an assignment at the parent tenant MUST authorise an
/// action evaluated at a descendant tenant, per `docs/DESIGN.md`. The companion
/// `get_subject_roles_sets_is_inherited_correctly` test pins the read side;
/// this pins the authorize side, where a per-grant strict-equality narrowing
/// would silently drop inherited grants.
///
/// Setup: `parent → child` chain via `FakeTenantResolverClient`, a
/// single assignment at `/tenants/{parent}`, and an evaluation request
/// at `/tenants/{child}`.
#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn evaluate_permission_allows_when_parent_tenant_grant_covers_child_tenant_request()
-> Result<()> {
    let harness = fresh_harness().await?;
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[parent, child]));
    let rg = Arc::new(FakeRbacRgRead::default());

    let role_id = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_id,
        "Reader",
        vec![PermissionRule::new("read", "gts.cf.test.example.vm.v1~")],
        Vec::new(),
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{parent}"),
    )
    .await;

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let result = evaluator
        .evaluate_permission(
            &sec,
            "alice",
            PrincipalType::User,
            "read",
            "gts.cf.test.example.vm.v1~",
            &Scope::Tenant { tenant_id: child },
        )
        .await
        .unwrap();
    match result {
        PermissionResult::Allowed(granted) => {
            assert_eq!(granted.grants.len(), 1);
            assert_eq!(granted.grants[0].role_definition_id, role_id);
            assert!(
                granted.grants[0].is_inherited,
                "parent-tenant grant evaluated at child MUST be marked is_inherited"
            );
        }
        other => panic!(
            "expected Allowed (ADR unconditional inheritance: parent grant authorises child), got {other:?}"
        ),
    }
    Ok(())
}

/// Companion to `evaluate_permission_allows_when_parent_tenant_grant_covers_child_tenant_request`:
/// a `Tenant{parent}` grant MUST also
/// authorise a request on an RG under a descendant tenant. The
/// inheritance flows through to RGs under any tenant in the chain.
#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn evaluate_permission_allows_when_parent_tenant_grant_covers_descendant_rg_request()
-> Result<()> {
    let harness = fresh_harness().await?;
    let parent = Uuid::new_v4();
    let child = Uuid::new_v4();
    let rg_id = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[parent, child]));
    let rg = Arc::new(FakeRbacRgRead::default());

    let role_id = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_id,
        "Reader",
        vec![PermissionRule::new("read", "gts.cf.test.example.vm.v1~")],
        Vec::new(),
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{parent}"),
    )
    .await;

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let result = evaluator
        .evaluate_permission(
            &sec,
            "alice",
            PrincipalType::User,
            "read",
            "gts.cf.test.example.vm.v1~",
            &Scope::ResourceGroup {
                tenant_id: child,
                group_id: rg_id,
            },
        )
        .await
        .unwrap();
    match result {
        PermissionResult::Allowed(granted) => {
            assert_eq!(granted.grants.len(), 1);
            assert!(
                granted.grants[0].is_inherited,
                "parent-tenant grant evaluated at descendant RG MUST be marked is_inherited"
            );
        }
        other => {
            panic!("expected Allowed for descendant RG under parent-tenant grant, got {other:?}")
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn evaluate_permission_denied_no_matching_permission() -> Result<()> {
    let harness = fresh_harness().await?;
    let ctx_tenant = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[ctx_tenant]));
    let rg = Arc::new(FakeRbacRgRead::default());

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let result = evaluator
        .evaluate_permission(
            &sec,
            "alice",
            PrincipalType::User,
            "read",
            "gts.cf.test.example.vm.v1~",
            &Scope::Tenant {
                tenant_id: ctx_tenant,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        result,
        PermissionResult::Denied(d) if d.reason == DenyReason::NoMatchingPermission
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn evaluate_permission_denied_not_permission_exclusion() -> Result<()> {
    let harness = fresh_harness().await?;
    let ctx_tenant = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[ctx_tenant]));
    let rg = Arc::new(FakeRbacRgRead::default());

    let role_id = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_id,
        "ReaderMinusSecrets",
        vec![PermissionRule::new("read", "gts.cf.test.example.vm.v1~")],
        vec![PermissionRule::new("read", "gts.cf.test.example.vm.v1~")],
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{ctx_tenant}"),
    )
    .await;

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let result = evaluator
        .evaluate_permission(
            &sec,
            "alice",
            PrincipalType::User,
            "read",
            "gts.cf.test.example.vm.v1~",
            &Scope::Tenant {
                tenant_id: ctx_tenant,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        result,
        PermissionResult::Denied(d) if d.reason == DenyReason::NotPermissionExclusion
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn evaluate_permission_additive_cross_role_isolation() -> Result<()> {
    let harness = fresh_harness().await?;
    let ctx_tenant = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[ctx_tenant]));
    let rg = Arc::new(FakeRbacRgRead::default());

    let role_a = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_a,
        "Granter",
        vec![PermissionRule::new("read", "gts.cf.test.example.vm.v1~")],
        Vec::new(),
    )
    .await;
    let role_b = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_b,
        "ExcluderWithBroad",
        vec![PermissionRule::new("*", "gts.cf.compute.*")],
        vec![PermissionRule::new("read", "gts.cf.test.example.vm.v1~")],
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_a,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{ctx_tenant}"),
    )
    .await;
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_b,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{ctx_tenant}"),
    )
    .await;

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();
    let result = evaluator
        .evaluate_permission(
            &sec,
            "alice",
            PrincipalType::User,
            "read",
            "gts.cf.test.example.vm.v1~",
            &Scope::Tenant {
                tenant_id: ctx_tenant,
            },
        )
        .await
        .unwrap();
    match result {
        PermissionResult::Allowed(granted) => {
            assert_eq!(granted.grants.len(), 1, "only Role-A grants this");
            assert_eq!(granted.grants[0].role_definition_id, role_a);
        }
        other => panic!("expected Allowed via Role-A, got {other:?}"),
    }
    Ok(())
}

// Regression: cross-RG isolation. Prior to the per-scope narrowing in
// `evaluate_permission`, a role assigned at one RG would also satisfy a
// request on a sibling RG in the same tenant — `get_subject_roles`
// returns every RG-scoped row in the context tenant (one `LIKE` query),
// and the evaluator did not filter by `assignment.scope`. The four
// assertions below pin down the applicability matrix the fix relies on.
#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn evaluate_permission_rg_scoped_role_does_not_leak_to_sibling_rg() -> Result<()> {
    let harness = fresh_harness().await?;
    let ctx_tenant = Uuid::new_v4();
    let rg_a = Uuid::new_v4();
    let rg_b = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[ctx_tenant]));
    let rg = Arc::new(FakeRbacRgRead::default());

    let role_id = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_id,
        "RgReader",
        vec![PermissionRule::new("read", "gts.cf.test.example.vm.v1~")],
        Vec::new(),
    )
    .await;
    // Grant `alice` the role at RG_A only.
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{ctx_tenant}/resourceGroups/{rg_a}"),
    )
    .await;

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let sec = toolkit_security::SecurityContext::anonymous();

    // Request at RG_A → Allowed (exact match).
    let allowed = evaluator
        .evaluate_permission(
            &sec,
            "alice",
            PrincipalType::User,
            "read",
            "gts.cf.test.example.vm.v1~",
            &Scope::ResourceGroup {
                tenant_id: ctx_tenant,
                group_id: rg_a,
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(allowed, PermissionResult::Allowed(_)),
        "expected Allowed at the granted RG, got {allowed:?}"
    );

    // Request at sibling RG_B remains denied, pinning that collection-context
    // evaluation does not make the assignment apply to a sibling group.
    let denied = evaluator
        .evaluate_permission(
            &sec,
            "alice",
            PrincipalType::User,
            "read",
            "gts.cf.test.example.vm.v1~",
            &Scope::ResourceGroup {
                tenant_id: ctx_tenant,
                group_id: rg_b,
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(denied, PermissionResult::Denied(ref d) if d.reason == DenyReason::NoMatchingPermission),
        "expected Denied at sibling RG, got {denied:?}"
    );

    // Request at the tenant root → Allowed, but narrowed to the granted
    // group's subtree. An RG grant from the request's own tenant does
    // contribute its permission (a hint-less collection read evaluates at the
    // caller's tenant), and `determine_scope_type` classifies the result as
    // `GroupSubtree` so a constraints-honouring PEP sees only that group's
    // members. What must never happen is a widening to tenant-scope
    // visibility — which is what the scope_type assertion below pins.
    let tenant_request = evaluator
        .evaluate_permission(
            &sec,
            "alice",
            PrincipalType::User,
            "read",
            "gts.cf.test.example.vm.v1~",
            &Scope::Tenant {
                tenant_id: ctx_tenant,
            },
        )
        .await
        .unwrap();
    let PermissionResult::Allowed(granted) = tenant_request else {
        panic!("expected Allowed-with-constraints at tenant root, got {tenant_request:?}");
    };
    // Exactly one assignment contributes, and the aggregate scope is derived
    // from it rather than from the request's own tenant.
    assert_eq!(granted.grants.len(), 1);
    match &granted.scope_type {
        PermissionScopeType::GroupSubtree { root_group_ids } => assert_eq!(
            root_group_ids,
            &vec![rg_a],
            "the tenant-scope answer MUST be constrained to the granted group alone; \
             a sibling group or a tenant-wide scope_type here is the escalation this \
             test guards"
        ),
        other => panic!(
            "an RG grant answering a tenant-scope request MUST stay a GroupSubtree; \
             got {other:?}, which would hand the PEP tenant-wide visibility"
        ),
    }

    Ok(())
}

// Regression: tenant-scoped grants still inherit DOWN to RG requests
// after the per-scope narrowing change. Without this, the fix would
// over-correct and break the documented inheritance model.
#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn evaluate_permission_tenant_grant_applies_to_rg_request() -> Result<()> {
    let harness = fresh_harness().await?;
    let ctx_tenant = Uuid::new_v4();
    let rg = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[ctx_tenant]));
    let rg_fake = Arc::new(FakeRbacRgRead::default());

    let role_id = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_id,
        "TenantReader",
        vec![PermissionRule::new("read", "gts.cf.test.example.vm.v1~")],
        Vec::new(),
    )
    .await;
    // Grant at the tenant scope.
    seed_assignment(
        &harness.provider.conn()?,
        &harness.assignment_repo,
        role_id,
        "alice",
        PrincipalType::User,
        &format!("/tenants/{ctx_tenant}"),
    )
    .await;

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg_fake,
    );
    let sec = toolkit_security::SecurityContext::anonymous();

    // Request at an RG under that tenant → Allowed (inherits down).
    let result = evaluator
        .evaluate_permission(
            &sec,
            "alice",
            PrincipalType::User,
            "read",
            "gts.cf.test.example.vm.v1~",
            &Scope::ResourceGroup {
                tenant_id: ctx_tenant,
                group_id: rg,
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(result, PermissionResult::Allowed(_)),
        "tenant grant must apply to a request at an RG under that tenant, got {result:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn evaluate_permission_invalid_stored_scope_surfaces() -> Result<()> {
    // Typed `Scope` at the storage boundary prevents corrupted-row
    // insertion via `repo.create(...)`. Drive the pure classifier
    // directly to exercise the `InvalidScope` surface.
    let harness = fresh_harness().await?;
    let ctx_tenant = Uuid::new_v4();
    let tenants = Arc::new(FakeTenantResolverClient::with_chain(&[ctx_tenant]));
    let rg = Arc::new(FakeRbacRgRead::default());

    let role_id = Uuid::now_v7();
    seed_role_definition(
        &harness.provider.conn()?,
        &harness.definition_repo,
        role_id,
        "Reader",
        vec![PermissionRule::new("read", "gts.cf.test.example.vm.v1~")],
        Vec::new(),
    )
    .await;
    // Insert a well-formed but nil-UUID RG scope; the classifier is
    // exercised below with a directly-malformed path.
    let bad_scope = rbac_sdk::models::Scope::ResourceGroup {
        tenant_id: Uuid::nil(),
        group_id: Uuid::nil(),
    };
    let _row: RoleAssignmentModel = harness
        .assignment_repo
        .create(
            &harness.provider.conn()?,
            NewRoleAssignment {
                role_definition_id: role_id,
                principal_id: "alice".to_owned(),
                principal_type: PrincipalType::User,
                scope: bad_scope,
                created_by: "system".to_owned(),
                // The author identity is a display-path concern; these
                // fixtures seed rows directly and record none.
                created_by_type: None,
                created_by_tenant_id: None,
            },
        )
        .await
        .expect("repo accepts well-formed scope rows");

    let evaluator = make_evaluator(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        tenants,
        rg,
    );
    let malformed_path = "/not/a/valid/scope";
    let err = determine_scope_type(malformed_path).unwrap_err();
    assert!(
        matches!(err, ScopeClassifyError::InvalidScope { ref scope } if scope == malformed_path)
    );
    let _ = evaluator;
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn malformed_scope_is_rejected() {
    let cases = [
        "/unknown/path",
        "",
        "/tenants/",
        "/tenants/not-a-uuid",
        "/tenants/00000000-0000-0000-0000-000000000000/resourceGroups/",
        "/tenants/00000000-0000-0000-0000-000000000000/resourceGroups/not-a-uuid",
        "/tenants/00000000-0000-0000-0000-000000000000/resourceGroups/00000000-0000-0000-0000-000000000000/extra",
    ];
    for case in cases {
        let result = determine_scope_type(case);
        assert!(
            matches!(
                &result,
                Err(ScopeClassifyError::InvalidScope { scope }) if scope == case
            ),
            "expected InvalidScope({case:?}), got {result:?}"
        );
    }
}
