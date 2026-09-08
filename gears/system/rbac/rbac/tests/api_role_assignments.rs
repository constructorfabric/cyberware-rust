//! API-level tests for `/rbac/v1/role-assignments` against a
//! freshly-migrated PostgreSQL testcontainer, with a mock enforcer +
//! fake tenant resolver + fake rg. One request per test via
//! `tower::ServiceExt::oneshot`.

#![cfg(test)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rbac_sdk::models::PrincipalType;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use tower::ServiceExt;
use uuid::Uuid;

use rbac::api::rest::role_assignments::{ApiState, router};
use rbac::domain::policy_enforcer::{
    Decision, MatchPred, MockPolicyEnforcer, ReadableScopes, ReadableScopesPred,
};
use rbac::domain::role_assignment::RoleAssignmentService;
use rbac::domain::role_assignment_repo::RoleAssignmentRepository;
use rbac::domain::role_definition_repo::{NewRoleDefinition, RoleDefinitionRepository};
use rbac::domain::scope_validator::ScopeValidator;
use rbac::infra::storage::role_assignment_repo;
use rbac::infra::storage::role_definition_repo;

mod common;
use common::scope_fakes as fakes;
use common::with_test_security_context;

struct Bits {
    router: Router,
    /// Connection source for tests that seed rows directly. The repos own none.
    provider: toolkit_db::DBProvider<toolkit_db::DbError>,
    assignment_repo: Arc<role_assignment_repo::RoleAssignmentRepository>,
    role_repo: Arc<role_definition_repo::RoleDefinitionRepository>,
    /// Held so the testcontainer survives the test scope.
    _fixture: common::PostgresUnderTest,
}

async fn build_router_with_policy(
    tenants: &[Uuid],
    rg: Arc<fakes::FakeRbacRgRead>,
    policy: Arc<MockPolicyEnforcer>,
) -> Result<Bits> {
    build_router_with_policy_and_branches(&[tenants], rg, policy).await
}

/// The body of [`build_router_with_policy`], taking the tenant hierarchy as
/// branches (each a root-to-leaf chain; branches sharing a first element
/// share that parent) rather than one chain. Tests that need two tenants
/// which are NOT each other's ancestors must use this — a chain would make
/// the second tenant a descendant of the first, and a descendant is inside
/// the first tenant's assignable subtree.
async fn build_router_with_policy_and_branches(
    tenant_branches: &[&[Uuid]],
    rg: Arc<fakes::FakeRbacRgRead>,
    policy: Arc<MockPolicyEnforcer>,
) -> Result<Bits> {
    let fixture = common::bring_up_migrated_postgres().await?;
    // Two independent `DBProvider`s so each repo owns a self-contained handle.
    let db_assignments = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let provider_assignments: DBProvider<DbError> = DBProvider::new(db_assignments);
    let db_definitions = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let _provider_definitions: DBProvider<DbError> = DBProvider::new(db_definitions);
    let assignment_repo = Arc::new(role_assignment_repo::RoleAssignmentRepository);
    let role_repo = Arc::new(role_definition_repo::RoleDefinitionRepository);
    let tenant_resolver = Arc::new(fakes::FakeTenantResolverClient::with_disjoint_subtrees(
        tenant_branches,
    )) as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let scope_validator = Arc::new(ScopeValidator::new(
        tenant_resolver,
        rg.clone() as Arc<dyn rbac::domain::rg_port::RbacRgRead>,
    ));
    let service = Arc::new(RoleAssignmentService::new(
        provider_assignments.clone(),
        Arc::clone(&assignment_repo),
        Arc::clone(&role_repo),
        policy,
        scope_validator,
        rg as Arc<dyn rbac::domain::rg_port::RbacRgRead>,
    ));
    let state = Arc::new(ApiState { service });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    Ok(Bits {
        router: with_test_security_context(router(state, &openapi)),
        provider: provider_assignments.clone(),
        assignment_repo,
        role_repo,
        _fixture: fixture,
    })
}

async fn build_router(tenants: &[Uuid], rg: Arc<fakes::FakeRbacRgRead>) -> Result<Bits> {
    let policy = Arc::new(MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Unrestricted,
    )]));
    build_router_with_policy(tenants, rg, policy).await
}

/// Sister of [`build_router`] for tenants that must be unrelated.
async fn build_router_disjoint(
    tenant_branches: &[&[Uuid]],
    rg: Arc<fakes::FakeRbacRgRead>,
) -> Result<Bits> {
    let policy = Arc::new(MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Unrestricted,
    )]));
    build_router_with_policy_and_branches(tenant_branches, rg, policy).await
}

/// Seed a custom role definition through the SeaORM repo. Returns id.
/// `scopes` is taken as wire-form strings for caller convenience; each
/// is parsed into the typed [`rbac_sdk::models::Scope`] expected by
/// [`NewRoleDefinition`].
async fn seed_role(
    conn: &toolkit_db::secure::DbConn<'_>,
    repo: &Arc<role_definition_repo::RoleDefinitionRepository>,
    tenant: Uuid,
    scopes: Vec<String>,
) -> Uuid {
    let id = Uuid::now_v7();
    let assignable_scopes: Vec<rbac_sdk::models::Scope> = scopes
        .iter()
        .map(|s| rbac_sdk::models::Scope::parse(s).expect("seed_role: scope must parse"))
        .collect();
    repo.create(
        conn,
        NewRoleDefinition {
            id,
            name: format!("TestRole-{id}"),
            description: Some("seed".to_owned()),
            permissions: vec![rbac_sdk::models::PermissionRule::new(
                "read",
                "gts.cf.resources.compute.vm.v1~",
            )],
            not_permissions: Vec::new(),
            assignable_scopes,
            owner_tenant_id: tenant,
            created_by: "tester".to_owned(),
        },
    )
    .await
    .expect("seed_role: SeaORM create must succeed");
    id
}

fn create_body(role_id: Uuid, scope: &str, principal_type: &str, principal_id: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "role_definition_id": role_id,
        "principal_id": principal_id,
        "principal_type": principal_type,
        "scope": scope,
    }))
    .expect("json")
}

async fn drain_status_and_body(
    response: axum::response::Response,
) -> (StatusCode, serde_json::Value) {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, body)
}

// ---------------------------------------------------------------------------
// A-19: happy create (User) + Location + ETag
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Malformed request inputs MUST surface as `application/problem+json`
// via the `CanonicalJson` / `CanonicalPath` wrappers (see role_assignments.rs
// handler signatures). Same contract as the role-definitions regressions.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn h4_post_role_assignment_with_malformed_json_returns_problem_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{invalid"))
                .expect("build req"),
        )
        .await
        .expect("send");
    let body = common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    let violations = body["context"]["field_violations"]
        .as_array()
        .expect("field_violations MUST be present");
    assert_eq!(violations[0]["field"], "body");
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn h4_delete_with_malformed_uuid_returns_problem_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/rbac/v1/role-assignments/not-a-uuid")
                .header(header::IF_MATCH, "irrelevant")
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    let body = common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    let violations = body["context"]["field_violations"]
        .as_array()
        .expect("field_violations MUST be present");
    assert_eq!(violations[0]["field"], "path");
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a19_happy_create_user_returns_201_with_location_and_etag() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant}"),
                    "User",
                    "alice",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get(header::LOCATION)
        .expect("Location header")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(location.starts_with("/rbac/v1/role-assignments/"));
    let etag = response
        .headers()
        .get(header::ETAG)
        .expect("ETag header")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(!etag.is_empty());
    Ok(())
}

// A-19b: happy create (ServicePrincipal)

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a19b_happy_create_service_principal() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant}"),
                    "ServicePrincipal",
                    "sp-1",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    Ok(())
}

// A-20: role not found → 404

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a20_role_not_found_returns_404() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let bogus_role = Uuid::now_v7();
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    bogus_role,
                    &format!("/tenants/{tenant}"),
                    "User",
                    "alice",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    common::assert_problem(response, StatusCode::NOT_FOUND, "not_found").await;
    Ok(())
}

// A-22: scope outside assignable_scopes → 400 (InvalidArgument).
#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a22_scope_outside_assignable_returns_400() -> Result<()> {
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    // Unrelated tenants: a chain would make tenant_b a child of tenant_a,
    // and a child IS inside tenant_a's assignable subtree.
    let bits = build_router_disjoint(&[&[tenant_a], &[tenant_b]], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant_a,
        vec![format!("/tenants/{tenant_a}")],
    )
    .await;
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant_b}"),
                    "User",
                    "alice",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    let violations = body["context"]["field_violations"].as_array().expect(
        "Problem `context.field_violations` MUST be present for ScopeNotWithinAssignableScopes",
    );
    assert_eq!(violations.len(), 1, "exactly one scope violation expected");
    assert_eq!(violations[0]["field"], "scope");
    assert_eq!(
        violations[0]["reason"],
        "scope_not_within_assignable_scopes"
    );
    Ok(())
}

// A-22b/c/d: group validation

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a22b_group_exists_returns_201() -> Result<()> {
    let tenant = Uuid::now_v7();
    let group_id = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default().with_group(group_id, tenant));
    let bits = build_router(&[tenant], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant}"),
                    "Group",
                    &group_id.to_string(),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a22c_group_not_found_returns_404() -> Result<()> {
    let tenant = Uuid::now_v7();
    let absent_group = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant}"),
                    "Group",
                    &absent_group.to_string(),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    common::assert_problem(response, StatusCode::NOT_FOUND, "not_found").await;
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a22d_group_tenant_mismatch_is_collapsed_into_not_found_404() -> Result<()> {
    // A group that exists in a different tenant surfaces as 404, not 422,
    // so an authorised caller cannot enumerate the platform-wide group
    // catalog by tenant. The 422 variant stays in the SDK taxonomy for
    // future first-party flows that need the distinction.
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let group_id = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default().with_group(group_id, tenant_b));
    let bits = build_router(&[tenant_a, tenant_b], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant_a,
        vec!["/".to_owned()],
    )
    .await;
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant_a}"),
                    "Group",
                    &group_id.to_string(),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    common::assert_problem(response, StatusCode::NOT_FOUND, "not_found").await;
    Ok(())
}

// A-23: duplicate → 409

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a23_duplicate_returns_409() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let body_bytes = create_body(role_id, &format!("/tenants/{tenant}"), "User", "alice");
    let first = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body_bytes.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    let dup = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body_bytes))
                .unwrap(),
        )
        .await
        .unwrap();
    common::assert_problem(dup, StatusCode::CONFLICT, "already_exists").await;
    Ok(())
}

// Invalid principal_type wire value → 400

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn invalid_principal_type_returns_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant}"),
                    "Robot",
                    "alice",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    let violations = body["context"]["field_violations"]
        .as_array()
        .expect("Problem `context.field_violations` MUST be present for InvalidPrincipalType");
    assert_eq!(
        violations.len(),
        1,
        "exactly one principal-type violation expected"
    );
    assert_eq!(violations[0]["field"], "principal_type");
    assert_eq!(violations[0]["reason"], "invalid_principal_type");
    Ok(())
}

// 403 on policy deny

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn create_policy_deny_returns_403() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let policy = Arc::new(MockPolicyEnforcer::match_table(vec![(
        MatchPred {
            operation: Some("write".to_owned()),
            target_type: Some("gts.cf.core.rbac.role_assignment.v1~".to_owned()),
            ..Default::default()
        },
        Decision::Deny,
    )]));
    let bits = build_router_with_policy(&[tenant], rg, policy).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant}"),
                    "User",
                    "alice",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    common::assert_problem(response, StatusCode::FORBIDDEN, "permission_denied").await;
    Ok(())
}

// ---------------------------------------------------------------------------
// A-24 .. A-27: list endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a24_list_no_readable_scopes_returns_200_empty_items() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    // Write allowed (to seed); readable_scopes empty → ReadableScopes::None.
    let policy = Arc::new(MockPolicyEnforcer::allow_all());
    let bits = build_router_with_policy(&[tenant], rg, policy).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    bits.router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant}"),
                    "User",
                    "alice",
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/rbac/v1/role-assignments")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = drain_status_and_body(response).await;
    assert_eq!(status, StatusCode::OK);
    let items = body.get("items").and_then(|v| v.as_array()).expect("items");
    assert!(
        items.is_empty(),
        "no-read caller MUST see empty items, got {items:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a26_list_with_filters_returns_narrowed_results() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    for principal in ["alice", "bob"] {
        bits.router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/rbac/v1/role-assignments")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(create_body(
                        role_id,
                        &format!("/tenants/{tenant}"),
                        "User",
                        principal,
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
    }
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/rbac/v1/role-assignments?%24filter=principal_id%20eq%20%27alice%27")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = drain_status_and_body(response).await;
    assert_eq!(status, StatusCode::OK);
    let items = body.get("items").and_then(|v| v.as_array()).expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["principal_id"], "alice");
    Ok(())
}

// `list_invalid_principal_type_query_returns_400` was retired with the
// OData migration: there's no `principal_type=<string>` query parameter
// any more — it's now an OData `$filter` expression like
// `principal_type eq 'User'`. A malformed enum value produces an empty
// result set, not a structured 400 violation (no domain-side enum gate).
// Surface that as a separate test if the contract needs to enforce
// closed-enum strings on `$filter` values.

// ---------------------------------------------------------------------------
// Get by id
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_authorized_returns_200_with_etag() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let create_resp = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant}"),
                    "User",
                    "alice",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let location = create_resp
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let create_etag = create_resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    let get_resp = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&location)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    let get_etag = get_resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(
        get_etag, create_etag,
        "ETag MUST be byte-stable across POST 201 / GET 200"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn get_unauthorized_returns_404_not_403() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    // Write allowed (so we can seed) — read denied.
    let policy = Arc::new(MockPolicyEnforcer::match_table(vec![
        (
            MatchPred {
                operation: Some("write".to_owned()),
                ..Default::default()
            },
            Decision::Allow,
        ),
        (
            MatchPred {
                operation: Some("read".to_owned()),
                ..Default::default()
            },
            Decision::Deny,
        ),
    ]));
    let bits = build_router_with_policy(&[tenant], rg, policy).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let create_resp = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant}"),
                    "User",
                    "alice",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::CREATED);
    let location = create_resp
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    let get_resp = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&location)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // 404, NOT 403 (don't leak existence to unauthorised callers).
    common::assert_problem(get_resp, StatusCode::NOT_FOUND, "not_found").await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a28_happy_delete_returns_204() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let create_resp = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant}"),
                    "User",
                    "alice",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let location = create_resp
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let etag = create_resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    let del_resp = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(&location)
                .header(header::IF_MATCH, etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_resp.status(), StatusCode::NO_CONTENT);
    Ok(())
}

// Canonical taxonomy has no 412 / 428: optimistic-concurrency variants
// surface as `FailedPrecondition` (HTTP 400). Callers branch on
// `context.violations[].type` (PRECONDITION_REQUIRED / PRECONDITION_FAILED).

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a28b_missing_if_match_returns_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let _ = bits
        .assignment_repo
        .create(
            &bits.provider.conn()?,
            rbac::domain::role_assignment_repo::NewRoleAssignment {
                role_definition_id: role_id,
                principal_id: "alice".to_owned(),
                principal_type: PrincipalType::User,
                scope: rbac_sdk::models::Scope::tenant(tenant),
                created_by: "tester".to_owned(),
                // The author identity is a display-path concern; these
                // fixtures seed rows directly and record none.
                created_by_type: None,
                created_by_tenant_id: None,
            },
        )
        .await
        .expect("seed");
    let any_id = bits
        .assignment_repo
        .list(
            &bits.provider.conn()?,
            rbac::domain::role_assignment_repo::VisibilityFilter::Unrestricted,
            &toolkit_odata::ODataQuery::new().with_limit(10),
        )
        .await
        .unwrap()
        .items[0]
        .id;
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/rbac/v1/role-assignments/{any_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body =
        common::assert_problem(response, StatusCode::BAD_REQUEST, "failed_precondition").await;
    let violations = body["context"]["violations"]
        .as_array()
        .expect("Problem `context.violations` MUST be present for OptimisticConcurrencyMissing");
    assert_eq!(
        violations.len(),
        1,
        "missing If-Match MUST carry exactly one precondition violation"
    );
    assert_eq!(violations[0]["type"], "PRECONDITION_REQUIRED");
    assert_eq!(violations[0]["subject"], "If-Match");
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a28c_stale_if_match_returns_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let create_resp = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-assignments")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_body(
                    role_id,
                    &format!("/tenants/{tenant}"),
                    "User",
                    "alice",
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let location = create_resp
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    // Parseable but stale ETag — `StaleEtag` maps to canonical
    // `FailedPrecondition` = HTTP 400 via
    // `RbacServiceError::OptimisticConcurrencyStale` (the
    // `OptimisticConcurrencyStale` arm in `src/api/rest/error.rs` calls
    // `failed_precondition()`); the 412-vs-400 distinction is carried by
    // `context.violations[].type == "PRECONDITION_FAILED"`, not the
    // status code.
    let stale = "1970-01-01T00:00:00.000000Z:00000000-0000-0000-0000-000000000000";
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(&location)
                .header(header::IF_MATCH, stale)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body =
        common::assert_problem(response, StatusCode::BAD_REQUEST, "failed_precondition").await;
    let violations = body["context"]["violations"]
        .as_array()
        .expect("Problem `context.violations` MUST be present for OptimisticConcurrencyStale");
    assert_eq!(
        violations.len(),
        1,
        "stale If-Match MUST carry exactly one precondition violation"
    );
    assert_eq!(violations[0]["type"], "PRECONDITION_FAILED");
    assert_eq!(violations[0]["subject"], "If-Match");
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a29_delete_not_found_returns_404() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let bogus = Uuid::now_v7();
    let stale = "1970-01-01T00:00:00.000000Z:00000000-0000-0000-0000-000000000000";
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/rbac/v1/role-assignments/{bogus}"))
                .header(header::IF_MATCH, stale)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    common::assert_problem(response, StatusCode::NOT_FOUND, "not_found").await;
    Ok(())
}

// ---------------------------------------------------------------------------
// PATCH is not mounted
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn patch_is_not_mounted_for_role_assignments() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;
    let any_id = Uuid::now_v7();
    let response = bits
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/rbac/v1/role-assignments/{any_id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b"{}".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.status() == StatusCode::METHOD_NOT_ALLOWED
            || response.status() == StatusCode::NOT_FOUND,
        "PATCH MUST NOT be mounted; got {:?}",
        response.status()
    );
    Ok(())
}
