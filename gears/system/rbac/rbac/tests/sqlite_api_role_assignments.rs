//! API-level tests for `/rbac/v1/role-assignments` driven against an
//! in-memory `SQLite` database instead of a Postgres testcontainer.
//!
//! Sister of `sqlite_api_role_definitions.rs`: mirrors the happy paths and
//! main branches of the `#[ignore]` Postgres suite in
//! `api_role_assignments.rs`, but runs without Docker as part of the default
//! `cargo test -p cf-gears-rbac` so it counts toward the gated coverage number.
//!
//! NOTE: the role-assignment and role-definition repos MUST share **one**
//! `DBProvider` — each `connect_db("sqlite::memory:")` opens a *separate*
//! in-memory database, so we clone a single provider (as `sqlite_smoke.rs`
//! does) to keep the FK between the two tables intact.

#![cfg(test)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;
use uuid::Uuid;

use rbac::api::rest::role_assignments::{ApiState, router};
use rbac::domain::metrics::NoopMetrics;
use rbac::domain::policy_enforcer::{
    Decision, MatchPred, MockPolicyEnforcer, ReadableScopes, ReadableScopesPred,
};
use rbac::domain::principal_name_reader::PrincipalNameReader;
use rbac::domain::principal_name_reader_mock::FakePrincipalNameReader;
use rbac::domain::role_assignment::{PrincipalNameHydrator, RoleAssignmentService};
use rbac::domain::role_assignment_repo::{NewRoleAssignment, RoleAssignmentRepository};
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
    role_repo: Arc<role_definition_repo::RoleDefinitionRepository>,
    /// The same repo the router writes through. Exposed so a test can seed
    /// a row the REST surface cannot produce — notably one with no recorded
    /// author identity, which is what every row written before the
    /// author-identity migration looks like.
    assignment_repo: Arc<role_assignment_repo::RoleAssignmentRepository>,
}

/// Build a router wired to a fresh in-memory SQLite DB shared by both repos.
///
/// `names` decides whether the service gets a display-name hydrator. It
/// takes the *reader* rather than a ready-made hydrator so the hydrator is
/// assembled over the very same RG and tenant-resolver fakes the rest of
/// the router uses — a hydrator wired to different fakes would resolve
/// against a different world than the one under test.
async fn build_router_with_policy(
    tenants: &[Uuid],
    rg: Arc<fakes::FakeRbacRgRead>,
    policy: Arc<MockPolicyEnforcer>,
    names: Option<Arc<FakePrincipalNameReader>>,
) -> Result<Bits> {
    build_router_with_policy_and_branches(&[tenants], rg, policy, names).await
}

/// The body of [`build_router_with_policy`], taking the tenant hierarchy as
/// branches (each a root-to-leaf chain; branches sharing a first element
/// share that parent) instead of a single chain.
async fn build_router_with_policy_and_branches(
    tenant_branches: &[&[Uuid]],
    rg: Arc<fakes::FakeRbacRgRead>,
    policy: Arc<MockPolicyEnforcer>,
    names: Option<Arc<FakePrincipalNameReader>>,
) -> Result<Bits> {
    let provider = common::fresh_sqlite_provider().await?;
    let assignment_repo = Arc::new(role_assignment_repo::RoleAssignmentRepository);
    let role_repo = Arc::new(role_definition_repo::RoleDefinitionRepository);
    let tenant_resolver = Arc::new(fakes::FakeTenantResolverClient::with_disjoint_subtrees(
        tenant_branches,
    )) as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg_read: Arc<dyn rbac::domain::rg_port::RbacRgRead> = rg;
    let scope_validator = Arc::new(ScopeValidator::new(
        Arc::clone(&tenant_resolver),
        Arc::clone(&rg_read),
    ));
    let service = RoleAssignmentService::new(
        provider.clone(),
        Arc::clone(&assignment_repo),
        Arc::clone(&role_repo),
        policy,
        scope_validator,
        Arc::clone(&rg_read),
    );
    // Hydration is additive: with no reader wired the service serves rows
    // exactly as it did before display names existed, which is what the
    // rest of this file asserts.
    let service = match names {
        None => service,
        Some(users) => service.with_hydrator(Arc::new(PrincipalNameHydrator::new(
            provider.clone(),
            users as Arc<dyn PrincipalNameReader>,
            Arc::clone(&rg_read),
            // The real repo over the same SQLite database the router writes
            // through, so a seeded role definition is the one the hydrator
            // reads its name from.
            Arc::clone(&role_repo),
            Arc::clone(&tenant_resolver),
            Arc::new(NoopMetrics),
        ))),
    };
    let state = Arc::new(ApiState {
        service: Arc::new(service),
    });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    Ok(Bits {
        router: with_test_security_context(router(state, &openapi)),
        provider,
        role_repo,
        assignment_repo,
    })
}

/// Default builder: allow-all policy with unrestricted readable scopes so
/// list/get return the created rows, and no hydrator.
async fn build_router(tenants: &[Uuid], rg: Arc<fakes::FakeRbacRgRead>) -> Result<Bits> {
    let policy = Arc::new(MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Unrestricted,
    )]));
    build_router_with_policy(tenants, rg, policy, None).await
}

/// Sister of [`build_router`] for tests that need tenants which are NOT
/// each other's ancestors. `build_router` seeds one root-to-leaf chain, so
/// its second tenant is a child of its first — fine for most tests, wrong
/// for anything asserting that an unrelated tenant is out of reach.
async fn build_router_disjoint(
    tenant_branches: &[&[Uuid]],
    rg: Arc<fakes::FakeRbacRgRead>,
) -> Result<Bits> {
    let policy = Arc::new(MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Unrestricted,
    )]));
    build_router_with_policy_and_branches(tenant_branches, rg, policy, None).await
}

/// Sister of [`build_router`] with display-name hydration wired over
/// `names`.
async fn build_router_with_names(
    tenants: &[Uuid],
    rg: Arc<fakes::FakeRbacRgRead>,
    names: Arc<FakePrincipalNameReader>,
) -> Result<Bits> {
    let policy = Arc::new(MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Unrestricted,
    )]));
    build_router_with_policy(tenants, rg, policy, Some(names)).await
}

/// Seed a custom role definition through the SeaORM repo; returns its id.
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

/// POST an assignment, asserting 201, and return (location, etag).
async fn create_assignment(
    bits: &Bits,
    role_id: Uuid,
    scope: &str,
    principal_type: &str,
    principal_id: &str,
) -> (String, String) {
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
                    scope,
                    principal_type,
                    principal_id,
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get(header::LOCATION)
        .expect("Location")
        .to_str()
        .unwrap()
        .to_owned();
    let etag = response
        .headers()
        .get(header::ETAG)
        .expect("ETag")
        .to_str()
        .unwrap()
        .to_owned();
    (location, etag)
}

/// GET the list endpoint with an already-encoded raw query string.
/// The neighbouring list tests inline their single URI; the query-surface
/// rejection test drives several strings through the same call, so it
/// gets a helper rather than four copies of the same builder.
async fn list_with_raw_query(bits: &Bits, query: &str) -> axum::response::Response {
    let uri = if query.is_empty() {
        "/rbac/v1/role-assignments".to_owned()
    } else {
        format!("/rbac/v1/role-assignments?{query}")
    };
    bits.router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

/// GET one absolute path off the router. The name-hydration tests fetch
/// single rows by `Location` and by id, so the request builder lives here
/// instead of being pasted three times.
async fn get_path(bits: &Bits, path: &str) -> axum::response::Response {
    bits.router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn create_user_returns_201_with_location_etag_and_body() -> Result<()> {
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
    assert!(response.headers().contains_key(header::ETAG));
    let location = response
        .headers()
        .get(header::LOCATION)
        .expect("Location")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(location.starts_with("/rbac/v1/role-assignments/"));
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let created: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(created["role_definition_id"], serde_json::json!(role_id));
    assert_eq!(created["principal_id"], "alice");
    assert_eq!(created["principal_type"], "User");
    assert_eq!(created["scope"], format!("/tenants/{tenant}"));
    Ok(())
}

#[tokio::test]
async fn create_service_principal_returns_201() -> Result<()> {
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
    let (_loc, _etag) = create_assignment(
        &bits,
        role_id,
        &format!("/tenants/{tenant}"),
        "ServicePrincipal",
        "sp-1",
    )
    .await;
    Ok(())
}

#[tokio::test]
async fn create_group_when_group_exists_returns_201() -> Result<()> {
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
    let (_loc, _etag) = create_assignment(
        &bits,
        role_id,
        &format!("/tenants/{tenant}"),
        "Group",
        &group_id.to_string(),
    )
    .await;
    Ok(())
}

#[tokio::test]
async fn create_group_when_group_missing_returns_404() -> Result<()> {
    let tenant = Uuid::now_v7();
    // Default rg has no groups → group lookup fails with NotFound.
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
                    &Uuid::now_v7().to_string(),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    common::assert_problem(response, StatusCode::NOT_FOUND, "not_found").await;
    Ok(())
}

#[tokio::test]
async fn create_with_unknown_role_returns_404() -> Result<()> {
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
                .body(Body::from(create_body(
                    Uuid::now_v7(),
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

#[tokio::test]
async fn create_scope_outside_assignable_returns_400() -> Result<()> {
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    // Disjoint subtrees: `build_router` seeds a parent-to-child CHAIN, which
    // would make tenant_b a descendant of tenant_a and therefore legally
    // assignable under the subtree rule. "Outside the envelope" has to mean
    // an unrelated tenant, so the two are seeded as separate roots.
    let bits = build_router_disjoint(&[&[tenant_a], &[tenant_b]], rg).await?;
    // Role is assignable only within tenant_a, but we target tenant_b.
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
    common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    Ok(())
}

#[tokio::test]
async fn create_duplicate_returns_409() -> Result<()> {
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
    let _ = create_assignment(
        &bits,
        role_id,
        &format!("/tenants/{tenant}"),
        "User",
        "alice",
    )
    .await;

    // Same (role, principal, scope) tuple again → duplicate.
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
    common::assert_problem(response, StatusCode::CONFLICT, "already_exists").await;
    Ok(())
}

#[tokio::test]
async fn get_authorized_returns_200() -> Result<()> {
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
    let (location, _etag) = create_assignment(
        &bits,
        role_id,
        &format!("/tenants/{tenant}"),
        "User",
        "alice",
    )
    .await;

    let response = bits
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
    let (status, body) = drain_status_and_body(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["principal_id"], "alice");
    Ok(())
}

#[tokio::test]
async fn list_returns_created_assignments() -> Result<()> {
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
    for p in ["alice", "bob"] {
        let _ = create_assignment(&bits, role_id, &format!("/tenants/{tenant}"), "User", p).await;
    }
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
    let items = body["items"].as_array().expect("items");
    assert_eq!(
        items.len(),
        2,
        "both assignments MUST be listed; body={body}"
    );
    Ok(())
}

#[tokio::test]
async fn list_with_filter_narrows_results() -> Result<()> {
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
    for p in ["alice", "bob"] {
        let _ = create_assignment(&bits, role_id, &format!("/tenants/{tenant}"), "User", p).await;
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
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["principal_id"], "alice");
    Ok(())
}

#[tokio::test]
async fn delete_returns_204() -> Result<()> {
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
    let (location, etag) = create_assignment(
        &bits,
        role_id,
        &format!("/tenants/{tenant}"),
        "User",
        "alice",
    )
    .await;

    let del = bits
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
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
    Ok(())
}

#[tokio::test]
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
    common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    Ok(())
}

#[tokio::test]
async fn malformed_json_returns_problem_400() -> Result<()> {
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
                .unwrap(),
        )
        .await
        .unwrap();
    common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    Ok(())
}

/// With no name hydration wired, the response carries no name keys at
/// all — *absent*, not `null`. This is the wire half of "hydration is
/// additive": switching it on adds keys, and its absence is invisible
/// beyond the missing keys. The 201 body is additionally never hydrated
/// by design — the write path performs no identity read — so the POST
/// response must stay nameless even once the hydrator exists.
#[tokio::test]
async fn response_omits_name_keys_when_no_hydrator_is_wired() -> Result<()> {
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
    let location = response
        .headers()
        .get(header::LOCATION)
        .expect("Location")
        .to_str()
        .unwrap()
        .to_owned();
    let (status, created) = drain_status_and_body(response).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        created.get("principal_name").is_none(),
        "absent, not null: {created}"
    );
    assert!(
        created.get("created_by_name").is_none(),
        "absent, not null: {created}"
    );

    // The single-row read goes through the hydrating service method, so
    // assert the same absence there rather than only on the create path.
    let fetched = bits
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
    let (status, body) = drain_status_and_body(fetched).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("principal_name").is_none(),
        "absent, not null: {body}"
    );
    assert!(
        body.get("created_by_name").is_none(),
        "absent, not null: {body}"
    );

    // …and on the list page, whose rows travel the batched path.
    let listed = list_with_raw_query(&bits, "").await;
    let (status, page) = drain_status_and_body(listed).await;
    assert_eq!(status, StatusCode::OK);
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert!(
        items[0].get("principal_name").is_none(),
        "absent, not null: {page}"
    );
    assert!(
        items[0].get("created_by_name").is_none(),
        "absent, not null: {page}"
    );
    Ok(())
}

/// Display names are resolved after pagination, so they are deliberately
/// not `RoleAssignmentFilterField` variants: a `$filter` or `$orderby`
/// naming one is the standard unknown-field 400, never a silently
/// ignored clause (which would break the page contract).
#[tokio::test]
async fn filtering_or_ordering_by_name_is_rejected() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let bits = build_router(&[tenant], rg).await?;

    for query in [
        "%24filter=principal_name%20eq%20%27x%27",
        "%24filter=created_by_name%20eq%20%27x%27",
        "%24filter=role_definition_name%20eq%20%27x%27",
        "%24orderby=principal_name",
        "%24orderby=created_by_name",
        "%24orderby=role_definition_name",
    ] {
        let response = list_with_raw_query(&bits, query).await;
        // `assert_problem` checks the status, the `application/problem+json`
        // content type and the canonical `type` slug in one go, so the
        // query string that produced a wrong status shows up in its
        // panic message alongside the body.
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "query {query} MUST be rejected as an unknown field"
        );
        common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Display-name hydration end to end
// ---------------------------------------------------------------------------
//
// The router built by `build_router_with_names` is the only one in this file
// with a hydrator attached, so these two tests are also the guard that the
// rest of the file's "no name keys at all" assertions are about an *unwired*
// hydrator rather than a broken one.

/// The identity the shared test `SecurityContext` authenticates as (see
/// `common::with_test_security_context`). `create` stamps this subject as
/// the row's author, together with its home tenant, so a test that wants a
/// resolvable author name has to seed exactly this pair.
const AUTHOR_SUBJECT: &str = "11111111-2222-3333-4444-555555555555";
const AUTHOR_TENANT: Uuid = uuid::uuid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

/// Both names reach the wire, on the single-row read and on the list page.
///
/// The author's home tenant is deliberately *not* the assignment's tenant:
/// a hydrator that looked the author up in the row's scope tenant — the
/// heuristic it legitimately uses for the role *holder* — would find nothing
/// and this test would fail. That is the whole point of storing the author's
/// tenant on the row.
#[tokio::test]
async fn hydrated_response_names_the_principal_and_the_author() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let names = Arc::new(
        FakePrincipalNameReader::default()
            .with_name(tenant, "alice", "Ada Lovelace")
            .with_name(AUTHOR_TENANT, AUTHOR_SUBJECT, "Grace Hopper"),
    );
    let bits = build_router_with_names(&[tenant], rg, names).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    let (location, _etag) = create_assignment(
        &bits,
        role_id,
        &format!("/tenants/{tenant}"),
        "User",
        "alice",
    )
    .await;

    let (status, body) = drain_status_and_body(get_path(&bits, &location).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["principal_name"], "Ada Lovelace", "body={body}");
    assert_eq!(body["created_by_name"], "Grace Hopper", "body={body}");
    // A name decorates the id, it never replaces it: a client that resolves
    // principals itself must still find the identifier it needs.
    assert_eq!(body["principal_id"], "alice", "body={body}");
    assert_eq!(body["created_by"], AUTHOR_SUBJECT, "body={body}");

    // The batched list path carries the same two names.
    let (status, page) = drain_status_and_body(list_with_raw_query(&bits, "").await).await;
    assert_eq!(status, StatusCode::OK);
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["principal_name"], "Ada Lovelace", "page={page}");
    assert_eq!(items[0]["created_by_name"], "Grace Hopper", "page={page}");
    Ok(())
}

/// A row with NULL author columns — every row written before the
/// author-identity migration, and any machine-authored row — is served with
/// its `created_by` id and no `created_by_name` key.
///
/// Two rows are seeded on purpose. The first proves the author branch
/// degrades *on its own*: its holder still resolves, so the missing author
/// name cannot be explained away by "the hydrator never ran". The second
/// resolves nothing at all and shows the requested shape — neither name key
/// present, absent rather than `null`.
#[tokio::test]
async fn row_without_recorded_author_identity_serves_no_author_name() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let names =
        Arc::new(FakePrincipalNameReader::default().with_name(tenant, "alice", "Ada Lovelace"));
    let bits = build_router_with_names(&[tenant], rg, names).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;

    // Written straight through the repo: the REST surface cannot produce
    // this shape, because `create` always stamps the calling subject.
    let named_holder = bits
        .assignment_repo
        .create(
            &bits.provider.conn()?,
            NewRoleAssignment {
                role_definition_id: role_id,
                principal_id: "alice".to_owned(),
                principal_type: rbac_sdk::models::PrincipalType::User,
                scope: rbac_sdk::models::Scope::tenant(tenant),
                created_by: "platform-bootstrap".to_owned(),
                created_by_type: None,
                created_by_tenant_id: None,
            },
        )
        .await
        .expect("seed a row with no recorded author identity");
    let unnamed_holder = bits
        .assignment_repo
        .create(
            &bits.provider.conn()?,
            NewRoleAssignment {
                role_definition_id: role_id,
                principal_id: "nobody".to_owned(),
                principal_type: rbac_sdk::models::PrincipalType::User,
                scope: rbac_sdk::models::Scope::tenant(tenant),
                created_by: "platform-bootstrap".to_owned(),
                created_by_type: None,
                created_by_tenant_id: None,
            },
        )
        .await
        .expect("seed a row that resolves nothing");

    let (status, body) = drain_status_and_body(
        get_path(
            &bits,
            &format!("/rbac/v1/role-assignments/{}", named_holder.id),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["principal_name"], "Ada Lovelace",
        "the hydrator MUST still have run: {body}"
    );
    assert!(
        body.get("created_by_name").is_none(),
        "absent, not null: {body}"
    );
    assert_eq!(body["created_by"], "platform-bootstrap", "body={body}");

    let (status, body) = drain_status_and_body(
        get_path(
            &bits,
            &format!("/rbac/v1/role-assignments/{}", unnamed_holder.id),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("principal_name").is_none(),
        "absent, not null: {body}"
    );
    assert!(
        body.get("created_by_name").is_none(),
        "absent, not null: {body}"
    );
    Ok(())
}

/// The role name rides the same hydration pass as the two principal names
/// and reaches both read shapes.
///
/// It is the one name on the row that needs no upstream: it comes from
/// RBAC's own `role_definitions` table, which is why this test can assert a
/// concrete value without seeding an identity fake for it. The fake user
/// reader is still wired, because the hydrator only runs when hydration is
/// switched on at all.
#[tokio::test]
async fn hydrated_response_names_the_role_definition() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let names = Arc::new(FakePrincipalNameReader::default());
    let bits = build_router_with_names(&[tenant], rg, names).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        tenant,
        vec![format!("/tenants/{tenant}")],
    )
    .await;
    // `seed_role` derives the name from the id, so the expectation is
    // computable rather than hard-coded.
    let expected_name = format!("TestRole-{role_id}");
    let (location, _etag) = create_assignment(
        &bits,
        role_id,
        &format!("/tenants/{tenant}"),
        "User",
        "alice",
    )
    .await;

    let (status, body) = drain_status_and_body(get_path(&bits, &location).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["role_definition_name"], expected_name, "body={body}");
    // The name decorates the id; a client that resolves roles itself must
    // still find the identifier.
    assert_eq!(
        body["role_definition_id"],
        serde_json::json!(role_id),
        "body={body}"
    );

    let (status, page) = drain_status_and_body(list_with_raw_query(&bits, "").await).await;
    assert_eq!(status, StatusCode::OK);
    let items = page["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0]["role_definition_name"], expected_name,
        "page={page}"
    );
    Ok(())
}

/// Without a hydrator the key is absent rather than `null` — the same
/// additive contract the other two names have.
#[tokio::test]
async fn role_definition_name_is_absent_when_no_hydrator_is_wired() -> Result<()> {
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
    let (location, _etag) = create_assignment(
        &bits,
        role_id,
        &format!("/tenants/{tenant}"),
        "User",
        "alice",
    )
    .await;

    let (status, body) = drain_status_and_body(get_path(&bits, &location).await).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("role_definition_name").is_none(),
        "absent, not null: {body}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// $filter spellings and the newly filterable created_by
// ---------------------------------------------------------------------------

/// One response carries a `text` `principal_id` and a `uuid`
/// `role_definition_id`, and before normalization each demanded a different
/// literal spelling — a bare UUID 400'd on the first, a quoted one on the
/// second. Both spellings now work on both fields, asserted end to end
/// through the real router, parser and SQLite query rather than at the AST
/// level where the unit tests live.
#[tokio::test]
async fn both_uuid_spellings_are_accepted_for_both_id_fields() -> Result<()> {
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
    // A UUID-shaped principal id: the shape a caller who holds a Keycloak
    // subject id actually has, and the only one where the two spellings of
    // `principal_id` are both plausible.
    let principal = Uuid::now_v7();
    let _ = create_assignment(
        &bits,
        role_id,
        &format!("/tenants/{tenant}"),
        "User",
        &principal.to_string(),
    )
    .await;

    for query in [
        // One id field spelled bare, the other quoted…
        format!("%24filter=principal_id%20eq%20{principal}"),
        format!("%24filter=role_definition_id%20eq%20%27{role_id}%27"),
        // …and the mirror image: every combination must be accepted.
        format!("%24filter=principal_id%20eq%20%27{principal}%27"),
        format!("%24filter=role_definition_id%20eq%20{role_id}"),
    ] {
        let (status, body) = drain_status_and_body(list_with_raw_query(&bits, &query).await).await;
        assert_eq!(status, StatusCode::OK, "query {query} must be accepted");
        let items = body["items"].as_array().expect("items");
        assert_eq!(items.len(), 1, "query {query} must match the row; {body}");
        assert_eq!(items[0]["principal_id"], principal.to_string());
    }
    Ok(())
}

/// Substring predicates on `principal_id` are why it stays a string field,
/// so the normalizer must not have cost them. A `startswith` on a UUID
/// prefix is the realistic use.
#[tokio::test]
async fn substring_filter_on_principal_id_still_narrows() -> Result<()> {
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
    for p in ["alice-eu", "bob-us"] {
        let _ = create_assignment(&bits, role_id, &format!("/tenants/{tenant}"), "User", p).await;
    }

    let (status, body) = drain_status_and_body(
        list_with_raw_query(&bits, "%24filter=contains(principal_id,%20%27alice%27)").await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "body={body}");
    assert_eq!(items[0]["principal_id"], "alice-eu");
    Ok(())
}

/// "Who granted these roles?" is now answerable from the query surface.
/// `created_by` is persisted and returned but was absent from the filter
/// field enum, so the question could only be answered by listing everything
/// and filtering client-side.
#[tokio::test]
async fn created_by_is_filterable() -> Result<()> {
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
    // Written through the REST surface, so `created_by` is the test
    // context's subject; and through the repo, so a second author exists to
    // be excluded.
    let _ = create_assignment(
        &bits,
        role_id,
        &format!("/tenants/{tenant}"),
        "User",
        "alice",
    )
    .await;
    bits.assignment_repo
        .create(
            &bits.provider.conn()?,
            NewRoleAssignment {
                role_definition_id: role_id,
                principal_id: "bob".to_owned(),
                principal_type: rbac_sdk::models::PrincipalType::User,
                scope: rbac_sdk::models::Scope::tenant(tenant),
                created_by: "platform-bootstrap".to_owned(),
                created_by_type: None,
                created_by_tenant_id: None,
            },
        )
        .await
        .expect("seed a row authored by someone else");

    let (status, body) = drain_status_and_body(
        list_with_raw_query(
            &bits,
            &format!("%24filter=created_by%20eq%20%27{AUTHOR_SUBJECT}%27"),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "body={body}");
    assert_eq!(items[0]["principal_id"], "alice");
    assert_eq!(items[0]["created_by"], AUTHOR_SUBJECT);

    // The other author's row is reachable by the same filter — the field
    // narrows, it does not just happen to match the caller.
    let (status, body) = drain_status_and_body(
        list_with_raw_query(
            &bits,
            "%24filter=created_by%20eq%20%27platform-bootstrap%27",
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "body={body}");
    assert_eq!(items[0]["principal_id"], "bob");
    Ok(())
}

/// `created_by` is a recognised field, so it is accepted in `$orderby` too
/// — the two clauses share one enum. Asserted rather than left implicit
/// because the cursor path needs a value extractor for every orderable
/// field, and a missing arm there is a runtime error, not a compile error.
///
/// Which is why the assertion has to cross a page boundary. `paginate_odata`
/// fetches `limit + 1` rows and mints `next_cursor` only when that extra row
/// came back, so a listing that fits on one page never builds a cursor and
/// never calls `extract_cursor_value` — the arm under test would stay
/// unexecuted, and binding (say) `Value::Uuid` against the `text` column
/// would pass here while breaking every real second page. Three rows read
/// two at a time force the cursor to be built from a `created_by` value and
/// then decoded back into the seekset predicate that serves page two.
#[tokio::test]
async fn created_by_is_orderable() -> Result<()> {
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
    // Seeded through the repo rather than the REST surface: `create`
    // stamps the caller's subject as the author, so every row written that
    // way shares one `created_by` and the ordering key would be constant.
    // Distinct authors make the cursor's `created_by` component actually
    // discriminate between the two pages.
    for (principal, author) in [
        ("alice", "author-alpha"),
        ("bob", "author-bravo"),
        ("carol", "author-charlie"),
    ] {
        bits.assignment_repo
            .create(
                &bits.provider.conn()?,
                NewRoleAssignment {
                    role_definition_id: role_id,
                    principal_id: principal.to_owned(),
                    principal_type: rbac_sdk::models::PrincipalType::User,
                    scope: rbac_sdk::models::Scope::tenant(tenant),
                    created_by: author.to_owned(),
                    created_by_type: None,
                    created_by_tenant_id: None,
                },
            )
            .await
            .expect("seed a row with a distinct author");
    }

    // Page one: ascending by author, two of the three rows, plus a cursor
    // because a third row exists.
    let page_one = list_with_raw_query(&bits, "limit=2&%24orderby=created_by").await;
    let (status, first) = drain_status_and_body(page_one).await;
    assert_eq!(status, StatusCode::OK, "body={first}");
    let first_items = first["items"].as_array().expect("items");
    assert_eq!(first_items.len(), 2, "body={first}");
    assert_eq!(first_items[0]["created_by"], "author-alpha");
    assert_eq!(first_items[1]["created_by"], "author-bravo");
    let cursor = first["page_info"]["next_cursor"]
        .as_str()
        .expect("a third row exists, so the page MUST carry a next_cursor")
        .to_owned();

    // Page two follows the cursor alone: `$orderby` alongside a cursor is
    // rejected by the extractor (the order is re-derived from the cursor's
    // own sort tokens), which is exactly the round trip through
    // `extract_cursor_value` this test exists for. The cursor is
    // base64url-no-pad, so it needs no further percent-encoding.
    let page_two = list_with_raw_query(&bits, &format!("limit=2&cursor={cursor}")).await;
    let (status, second) = drain_status_and_body(page_two).await;
    assert_eq!(status, StatusCode::OK, "body={second}");
    let second_items = second["items"].as_array().expect("items");
    // The remainder, and only the remainder: no duplicate of page one's
    // last row (a cursor value bound at the wrong shape typically re-serves
    // it) and no skipped row.
    assert_eq!(second_items.len(), 1, "body={second}");
    assert_eq!(second_items[0]["created_by"], "author-charlie");
    assert_eq!(second_items[0]["principal_id"], "carol");
    assert!(
        second["page_info"]["next_cursor"].is_null(),
        "the last page MUST NOT offer a further cursor: {second}"
    );

    // Spelled out as a set as well, so a future off-by-one in the seekset
    // fails on "which rows" rather than only on "how many".
    let seen: Vec<&str> = first_items
        .iter()
        .chain(second_items.iter())
        .map(|row| row["principal_id"].as_str().expect("principal_id"))
        .collect();
    assert_eq!(seen, ["alice", "bob", "carol"]);
    Ok(())
}

/// A role owned by a parent tenant CAN be assigned at a child tenant:
/// a tenant assignable scope covers its whole subtree.
///
/// `assignable_scopes_admit` answers same-tenant shapes structurally and
/// sends everything else to `ScopeValidator::is_ancestor`, which resolves
/// real ancestry through the tenant resolver — comparing the two tenant
/// ids for equality instead would reject every child.
///
/// The control assignment at the owner tenant keeps the pair honest: if
/// the envelope check were dropped entirely, both would pass, so the
/// unrelated-tenant rejection is pinned by
/// `create_scope_outside_assignable_returns_400` next to it.
#[tokio::test]
async fn create_at_a_child_tenant_of_the_owner_is_admitted() -> Result<()> {
    let parent = Uuid::now_v7();
    let child = Uuid::now_v7();
    // `build_router` seeds a chain, so `child` is a real child of `parent`.
    let bits = build_router(&[parent, child], Arc::new(fakes::FakeRbacRgRead::default())).await?;
    let role_id = seed_role(
        &bits.provider.conn()?,
        &bits.role_repo,
        parent,
        vec![format!("/tenants/{parent}")],
    )
    .await;

    // Control: at the owner tenant itself the very same role assigns fine.
    let _ = create_assignment(
        &bits,
        role_id,
        &format!("/tenants/{parent}"),
        "User",
        "alice",
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
                    &format!("/tenants/{child}"),
                    "User",
                    "bob",
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    let (status, body) = drain_status_and_body(response).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a child tenant is inside the owner tenant's subtree; body={body}"
    );
    assert_eq!(
        body["scope"],
        format!("/tenants/{child}"),
        "the assignment must be recorded at the child scope it was asked for; body={body}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Authorization-denial paths.
//
// Ported from `api_role_assignments.rs`, where all 21 tests are
// `#[ignore]`d behind Docker. The builders in this file wired
// `allow_all().with_readable_scopes(Unrestricted)` in every case, so the
// grant/revoke denial contract — and the deliberate 404-instead-of-403
// non-leakage rule — had no coverage in a default `cargo test` run.
// ---------------------------------------------------------------------------

/// The `resource_type` the assignment write path presents to the policy
/// enforcer. Mirrors the `pub(crate)`
/// `rbac::domain::model::resource_types::ROLE_ASSIGNMENT`, which an
/// integration test cannot reach. The `recorded_calls` assertion below is
/// what keeps this copy honest: if the handler ever presents a different
/// string, the test fails instead of silently falling through to
/// `match_table`'s Deny default.
const ROLE_ASSIGNMENT_TARGET_TYPE: &str = "gts.cf.core.rbac.role_assignment.v1~";

#[tokio::test]
async fn create_policy_deny_returns_403() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    let policy = Arc::new(MockPolicyEnforcer::match_table(vec![(
        MatchPred {
            operation: Some("write".to_owned()),
            target_type: Some(ROLE_ASSIGNMENT_TARGET_TYPE.to_owned()),
            ..Default::default()
        },
        Decision::Deny,
    )]));
    // `match_table`'s no-match default is also Deny, so a predicate that
    // never matched would still produce a 403 and the test would pass for
    // the wrong reason. Keep a handle and assert below that the enforcer
    // was actually consulted with this operation/target pair.
    let probe = Arc::clone(&policy);
    let bits = build_router_with_policy(&[tenant], rg, policy, None).await?;
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
    assert!(
        probe
            .recorded_calls()
            .iter()
            .any(|(_, _, operation, target_type, _)| {
                operation == "write" && target_type == ROLE_ASSIGNMENT_TARGET_TYPE
            }),
        "the 403 must come from the seeded predicate, not from the no-match default; \
         recorded calls were {:?}",
        probe.recorded_calls()
    );
    Ok(())
}

#[tokio::test]
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
    let bits = build_router_with_policy(&[tenant], rg, policy, None).await?;
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

#[tokio::test]
async fn a24_list_no_readable_scopes_returns_200_empty_items() -> Result<()> {
    let tenant = Uuid::now_v7();
    let rg = Arc::new(fakes::FakeRbacRgRead::default());
    // Write allowed (to seed); readable_scopes empty → ReadableScopes::None.
    let policy = Arc::new(MockPolicyEnforcer::allow_all());
    let bits = build_router_with_policy(&[tenant], rg, policy, None).await?;
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
