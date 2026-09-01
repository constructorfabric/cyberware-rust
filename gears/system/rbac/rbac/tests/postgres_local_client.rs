//! Postgres-backed integration tests for [`RbacServiceLocalClient`] —
//! the in-process adapter that publishes `dyn RbacServiceClientV1` in
//! `ClientHub`.

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
use rbac_sdk::api::RbacServiceClientV1;
use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::{
    DenyReason, EvaluatePermissionRequest, GetSubjectRolesRequest, PermissionResult,
    PermissionRule, PermissionScopeType, PrincipalType, Scope,
};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use rbac::api::service::local_client::RbacServiceLocalClient;
use rbac::domain::permission_evaluator::PermissionEvaluator;
use rbac::domain::role_assignment_repo::{NewRoleAssignment, RoleAssignmentRepository};
use rbac::domain::role_definition_repo::{NewRoleDefinition, RoleDefinitionRepository};
use rbac::infra::storage::role_assignment_repo;
use rbac::infra::storage::role_definition_repo;

use common::scope_fakes::{FakeRbacRgRead, FakeTenantResolverClient};

/// Build a non-anonymous `SecurityContext` scoped to `tenant`. This is the
/// shape every tenant-scoped REST caller's `SecurityContext` takes after
/// the AuthN middleware has resolved it.
fn tenant_ctx(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .subject_type("user")
        .build()
        .expect("test ctx with non-nil subject + tenant must build")
}

/// Build a first-party root `SecurityContext` (`token_scopes == ["*"]`)
/// homed at `home_tenant`. A `Scope::Root` lookup falls back to the caller's
/// home tenant (see `RbacServiceLocalClient::get_subject_roles`), so callers
/// seed `home_tenant` in the resolver for that fallback to resolve.
fn root_ctx(home_tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(home_tenant)
        .subject_type("service")
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("root ctx must build")
}

struct LocalClientHarness {
    /// The evaluator's connection source. Held here because the repos no
    /// longer own one: the executor is passed per call.
    provider: DBProvider<DbError>,
    assignment_repo: Arc<role_assignment_repo::RoleAssignmentRepository>,
    definition_repo: Arc<role_definition_repo::RoleDefinitionRepository>,
    _fixture: common::PostgresUnderTest,
}

async fn fresh_harness() -> Result<LocalClientHarness> {
    let fixture = common::bring_up_migrated_postgres().await?;
    let db_assignments = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let provider_assignments: DBProvider<DbError> = DBProvider::new(db_assignments);
    let db_definitions = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let provider_definitions: DBProvider<DbError> = DBProvider::new(db_definitions);

    // Both repos read through the same server; one provider is enough now
    // that neither owns a connection.
    let _ = provider_definitions;
    Ok(LocalClientHarness {
        provider: provider_assignments,
        assignment_repo: Arc::new(role_assignment_repo::RoleAssignmentRepository),
        definition_repo: Arc::new(role_definition_repo::RoleDefinitionRepository),
        _fixture: fixture,
    })
}

fn local_client(
    harness: &LocalClientHarness,
    ctx_tenant: Uuid,
) -> RbacServiceLocalClient<
    role_assignment_repo::RoleAssignmentRepository,
    role_definition_repo::RoleDefinitionRepository,
> {
    local_client_with_tenants(harness, &[ctx_tenant])
}

/// Like [`local_client`], but seeds the tenant resolver with every tenant in
/// `tenants` (each an independent root). Used by tests that address more than
/// one tenant in a single client — e.g. a root caller doing a cross-tenant
/// lookup — where evaluation calls `get_ancestors` on each target tenant.
fn local_client_with_tenants(
    harness: &LocalClientHarness,
    tenants: &[Uuid],
) -> RbacServiceLocalClient<
    role_assignment_repo::RoleAssignmentRepository,
    role_definition_repo::RoleDefinitionRepository,
> {
    let branches: Vec<&[Uuid]> = tenants.iter().map(std::slice::from_ref).collect();
    let resolver = Arc::new(FakeTenantResolverClient::with_disjoint_subtrees(&branches));
    let rg = Arc::new(FakeRbacRgRead::default());
    let evaluator = Arc::new(PermissionEvaluator::new(
        harness.provider.clone(),
        Arc::clone(&harness.assignment_repo),
        Arc::clone(&harness.definition_repo),
        resolver,
        rg,
        Arc::new(rbac::domain::metrics::NoopMetrics),
    ));
    RbacServiceLocalClient::new(evaluator)
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_subject_roles_returns_empty_on_unseeded_state() -> Result<()> {
    let ctx_tenant = Uuid::new_v4();
    let harness = fresh_harness().await?;
    let client = local_client(&harness, ctx_tenant);
    let request = GetSubjectRolesRequest::new(
        "subject-1",
        PrincipalType::User,
        Scope::tenant(ctx_tenant),
        false,
    );
    let ctx = tenant_ctx(ctx_tenant);
    let response = client
        .get_subject_roles(&ctx, request)
        .await
        .expect("wired adapter must return Ok");
    assert!(response.roles.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn evaluate_permission_returns_denied_no_matching_permission_on_unseeded_state() -> Result<()>
{
    let ctx_tenant = Uuid::new_v4();
    let harness = fresh_harness().await?;
    let client = local_client(&harness, ctx_tenant);
    let request = EvaluatePermissionRequest::new(
        "subject-1",
        PrincipalType::User,
        "read",
        Scope::tenant(ctx_tenant),
        "gts.cf.resources.compute.vm.v1~",
    );
    let ctx = tenant_ctx(ctx_tenant);
    let response = client
        .evaluate_permission(&ctx, request)
        .await
        .expect("wired adapter must return Ok");
    assert!(!response.allowed());
    assert!(matches!(
        response.result,
        PermissionResult::Denied(d) if d.reason == DenyReason::NoMatchingPermission
    ));
    Ok(())
}

/// End-to-end round trip: a seeded role + assignment yields
/// `PermissionResult::Allowed` through `evaluate_permission`.
#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn evaluate_permission_round_trip_allowed() -> Result<()> {
    let ctx_tenant = Uuid::new_v4();
    let harness = fresh_harness().await?;

    let role_id = Uuid::now_v7();
    let conn = harness.provider.conn()?;
    harness
        .definition_repo
        .create(
            &conn,
            NewRoleDefinition {
                id: role_id,
                name: "Reader".to_owned(),
                description: None,
                permissions: vec![PermissionRule::new("read", "gts.cf.test.example.vm.v1~")],
                not_permissions: Vec::new(),
                assignable_scopes: vec![Scope::Root],
                owner_tenant_id: Uuid::new_v4(),
                created_by: "system".to_owned(),
            },
        )
        .await
        .expect("seed role definition");
    harness
        .assignment_repo
        .create(
            &conn,
            NewRoleAssignment {
                role_definition_id: role_id,
                principal_id: "alice".to_owned(),
                principal_type: PrincipalType::User,
                scope: Scope::tenant(ctx_tenant),
                created_by: "system".to_owned(),
                // The author identity is a display-path concern; these
                // fixtures seed rows directly and record none.
                created_by_type: None,
                created_by_tenant_id: None,
            },
        )
        .await
        .expect("seed role assignment");

    let client: Arc<dyn RbacServiceClientV1> = Arc::new(local_client(&harness, ctx_tenant));
    let request = EvaluatePermissionRequest::new(
        "alice",
        PrincipalType::User,
        "read",
        Scope::tenant(ctx_tenant),
        "gts.cf.test.example.vm.v1~",
    );
    let ctx = tenant_ctx(ctx_tenant);
    let response = client.evaluate_permission(&ctx, request).await.expect("Ok");
    assert!(response.allowed());
    match response.result {
        PermissionResult::Allowed(granted) => {
            assert_eq!(granted.grants.len(), 1);
            assert!(matches!(
                granted.scope_type,
                PermissionScopeType::TenantSubtree { root_tenant_id } if root_tenant_id == ctx_tenant
            ));
        }
        other => panic!("expected Allowed, got {other:?}"),
    }
    Ok(())
}

// --- Caller-side authz gate regression tests ----------------------------

/// Anonymous `SecurityContext` must be rejected on both methods — the
/// adapter is the trust boundary, not the evaluator.
#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn anonymous_ctx_is_rejected() -> Result<()> {
    let ctx_tenant = Uuid::new_v4();
    let harness = fresh_harness().await?;
    let client = local_client(&harness, ctx_tenant);
    let ctx = SecurityContext::anonymous();

    let get_request = GetSubjectRolesRequest::new(
        "subject-1",
        PrincipalType::User,
        Scope::tenant(ctx_tenant),
        false,
    );
    let err = client
        .get_subject_roles(&ctx, get_request)
        .await
        .expect_err("anonymous caller must be denied");
    assert!(matches!(err, RbacServiceError::AuthorizationDenied { .. }));

    let eval_request = EvaluatePermissionRequest::new(
        "subject-1",
        PrincipalType::User,
        "read",
        Scope::tenant(ctx_tenant),
        "gts.cf.test.example.vm.v1~",
    );
    let err = client
        .evaluate_permission(&ctx, eval_request)
        .await
        .expect_err("anonymous caller must be denied");
    assert!(matches!(err, RbacServiceError::AuthorizationDenied { .. }));
    Ok(())
}

/// Tenant(A) caller asking about a subject in `Scope::Tenant(B)` is a
/// cross-tenant lookup and must be denied at the adapter, not silently
/// returning an empty set from the evaluator.
#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn tenant_caller_cross_tenant_request_is_denied() -> Result<()> {
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let harness = fresh_harness().await?;
    let client = local_client(&harness, tenant_b);
    let ctx = tenant_ctx(tenant_a);

    let request = EvaluatePermissionRequest::new(
        "subject-1",
        PrincipalType::User,
        "read",
        Scope::tenant(tenant_b),
        "gts.cf.test.example.vm.v1~",
    );
    let err = client
        .evaluate_permission(&ctx, request)
        .await
        .expect_err("cross-tenant request must be denied");
    assert!(matches!(err, RbacServiceError::AuthorizationDenied { .. }));
    Ok(())
}

/// Tenant(A) caller addressing `Scope::Root` is an escalation attempt and
/// must be denied — only first-party root callers may address `Root`.
#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn tenant_caller_root_scope_is_denied() -> Result<()> {
    let tenant_a = Uuid::new_v4();
    let harness = fresh_harness().await?;
    let client = local_client(&harness, tenant_a);
    let ctx = tenant_ctx(tenant_a);

    let request = GetSubjectRolesRequest::new("subject-1", PrincipalType::User, Scope::Root, false);
    let err = client
        .get_subject_roles(&ctx, request)
        .await
        .expect_err("tenant caller hitting Root must be denied");
    assert!(matches!(err, RbacServiceError::AuthorizationDenied { .. }));
    Ok(())
}

/// Legitimate-path regression: a Tenant(A) caller asking about a subject
/// in `Scope::Tenant(A)` is allowed through the gate (existing behaviour
/// must keep working — the gate is not over-blocking).
#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn tenant_caller_same_tenant_request_is_allowed() -> Result<()> {
    let ctx_tenant = Uuid::new_v4();
    let harness = fresh_harness().await?;
    let client = local_client(&harness, ctx_tenant);
    let ctx = tenant_ctx(ctx_tenant);

    let request = GetSubjectRolesRequest::new(
        "subject-1",
        PrincipalType::User,
        Scope::tenant(ctx_tenant),
        false,
    );
    let response = client
        .get_subject_roles(&ctx, request)
        .await
        .expect("same-tenant request must pass the gate");
    // Unseeded state → empty roles, mirroring `get_subject_roles_returns_empty_on_unseeded_state`.
    assert!(response.roles.is_empty());
    Ok(())
}

/// First-party root caller (`token_scopes == ["*"]`) may address any
/// scope, including a cross-tenant lookup and `Scope::Root`.
#[tokio::test]
#[ignore = "Phase 3 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn root_caller_any_scope_is_allowed() -> Result<()> {
    let root_home = Uuid::new_v4();
    let other_tenant = Uuid::new_v4();
    let harness = fresh_harness().await?;
    let ctx = root_ctx(root_home);
    // The fake resolver MUST know every tenant the evaluator will resolve:
    // `other_tenant` (the cross-tenant query) and the root caller's own
    // `subject_tenant_id` — the `Scope::Root` request falls back to the
    // caller's home tenant in `local_client::get_subject_roles`, whose
    // ancestor chain is then resolved.
    let client = local_client_with_tenants(&harness, &[other_tenant, ctx.subject_tenant_id()]);

    // Cross-tenant lookup.
    let cross_request = GetSubjectRolesRequest::new(
        "subject-1",
        PrincipalType::User,
        Scope::tenant(other_tenant),
        false,
    );
    let cross_response = client
        .get_subject_roles(&ctx, cross_request)
        .await
        .expect("root caller may address any tenant");
    assert!(cross_response.roles.is_empty());

    // Root scope.
    let root_request =
        GetSubjectRolesRequest::new("subject-1", PrincipalType::User, Scope::Root, false);
    let root_response = client
        .get_subject_roles(&ctx, root_request)
        .await
        .expect("root caller may address Root");
    assert!(root_response.roles.is_empty());
    Ok(())
}
