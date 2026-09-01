//! API-level tests for `/rbac/v1/role-definitions` driven against an
//! in-memory `SQLite` database instead of a Postgres testcontainer.
//!
//! These mirror the happy paths and main branches of the `#[ignore]`
//! Postgres suite in `api_role_definitions.rs`, but because the rbac
//! migrations are dual-mode and `SQLite` needs no Docker, they run as part
//! of the **default** `cargo test -p cf-gears-rbac` (and therefore count toward the
//! gated coverage number). Postgres-specific behaviour — serialization
//! conflicts, GIN/trigram index plans, `information_schema` introspection —
//! stays in the Docker-gated `postgres_*` / `api_*` suites.
//!
//! Assertions are deliberately backend-agnostic: they check `DomainError`
//! categories and HTTP/Problem contracts, never raw SQL error text (which
//! differs between `SQLite` and Postgres).

#![cfg(test)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;
use uuid::Uuid;

use rbac::api::rest::role_definitions::{ApiState, router};
use rbac::domain::policy_enforcer::{MockPolicyEnforcer, ReadableScopes, ReadableScopesPred};
use rbac::domain::role_assignment_repo::{NewRoleAssignment, RoleAssignmentRepository};
use rbac::domain::role_definition::RoleDefinitionService;
use rbac::domain::role_definition_repo::RoleDefinitionRepository;
use rbac::domain::scope_validator::ScopeValidator;
use rbac::domain::target_type_validator::{AcceptAllTargetTypeValidator, TargetTypeValidator};
use rbac::infra::storage::{role_assignment_repo, role_definition_repo};

mod common;
use common::scope_fakes as fakes;
use common::with_test_security_context;

/// No-op `RbacRgRead` — these tests use tenant-only scopes.
struct NoopRbacRgRead;
#[async_trait::async_trait]
impl rbac::domain::rg_port::RbacRgRead for NoopRbacRgRead {
    async fn get_group(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _id: Uuid,
    ) -> Result<rbac::domain::rg_port::RbacRgGroup, rbac::domain::rg_port::RbacRgReadError> {
        Err(rbac::domain::rg_port::RbacRgReadError::NotFound)
    }

    /// No groups, so no group names. Display-name resolution is not part
    /// of what this test exercises.
    async fn group_names(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, String>, rbac::domain::rg_port::RbacRgReadError>
    {
        Ok(std::collections::HashMap::new())
    }

    async fn list_memberships(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _query: &toolkit_odata::ODataQuery,
    ) -> Result<
        toolkit_odata::Page<rbac::domain::rg_port::RbacRgMembership>,
        rbac::domain::rg_port::RbacRgReadError,
    > {
        Ok(toolkit_odata::Page::new(
            Vec::new(),
            toolkit_odata::PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: 0,
            },
        ))
    }
}

/// Router plus the two repository handles behind it, for tests that seed
/// rows directly instead of going through the HTTP surface.
struct Harness {
    router: Router,
    /// Connection source for tests that seed rows directly. The repos own none.
    provider: toolkit_db::DBProvider<toolkit_db::DbError>,
    assignments: Arc<role_assignment_repo::RoleAssignmentRepository>,
}

/// Build a router wired to a fresh in-memory SQLite database. Both repos
/// share the one provider, so an assignment seeded through `assignments` is
/// visible to the count the role-definition reads take.
async fn build_harness(tenant: Uuid, policy: Arc<MockPolicyEnforcer>) -> Result<Harness> {
    let provider = common::fresh_sqlite_provider().await?;
    let repo = Arc::new(role_definition_repo::RoleDefinitionRepository);
    let assignments = Arc::new(role_assignment_repo::RoleAssignmentRepository);
    let tenant_resolver = Arc::new(fakes::FakeTenantResolverClient::with_chain(&[tenant]))
        as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg = Arc::new(NoopRbacRgRead) as Arc<dyn rbac::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, rg));
    let target_type_validator: Arc<dyn TargetTypeValidator> =
        Arc::new(AcceptAllTargetTypeValidator::new());
    let service = Arc::new(RoleDefinitionService::new(
        provider.clone(),
        Arc::clone(&repo),
        Arc::clone(&assignments),
        policy,
        scope_validator,
        target_type_validator,
    ));
    let state = Arc::new(ApiState { service });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    Ok(Harness {
        router: with_test_security_context(router(state, &openapi)),
        provider,
        assignments,
    })
}

/// Convenience: allow/deny policy.
async fn build_router(tenant: Uuid, allow: bool) -> Result<Router> {
    let policy = if allow {
        Arc::new(MockPolicyEnforcer::allow_all())
    } else {
        Arc::new(MockPolicyEnforcer::deny_all())
    };
    Ok(build_harness(tenant, policy).await?.router)
}

/// Policy that grants unrestricted read on **every** resource type, so both
/// the role-definition visibility and the assignment-count visibility are
/// wide open. `ReadableScopesPred::default()` matches any `target_type`,
/// which is the point: the count needs a second `readable_scopes` answer,
/// for `role_assignment`, that a role-definition-only table would not give.
fn policy_reading_everything() -> Arc<MockPolicyEnforcer> {
    Arc::new(MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Unrestricted,
    )]))
}

fn create_body(name: &str, tenant: Uuid) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": "test",
        "permissions": [
            { "operation": "read", "target_type": "gts.cf.resources.compute.vm.v1~" }
        ],
        "not_permissions": [],
        "assignable_scopes": [format!("/tenants/{tenant}")],
        "owner_tenant_id": tenant,
    })
}

/// POST a role definition and return (id, etag) from the 201 response.
async fn create_role(router: &Router, name: &str, tenant: Uuid) -> (String, String) {
    let body = serde_json::to_vec(&create_body(name, tenant)).expect("json");
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-definitions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::CREATED);
    let etag = response
        .headers()
        .get(header::ETAG)
        .expect("etag")
        .to_str()
        .expect("utf8")
        .to_owned();
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let id = value["id"].as_str().expect("id field").to_owned();
    (id, etag)
}

#[tokio::test]
async fn create_returns_201_with_location_etag_and_echoes_body() -> Result<()> {
    let tenant = Uuid::now_v7();
    let router = build_router(tenant, true).await?;
    let body = serde_json::to_vec(&create_body("Auditor", tenant)).expect("json");
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-definitions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(response.headers().contains_key(header::ETAG));
    assert!(response.headers().contains_key(header::LOCATION));
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let created: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(created["name"], "Auditor");
    assert_eq!(created["owner_tenant_id"], serde_json::json!(tenant));
    assert_eq!(
        created["permissions"],
        serde_json::json!([
            { "operation": "read", "target_type": "gts.cf.resources.compute.vm.v1~" }
        ])
    );
    Ok(())
}

#[tokio::test]
async fn get_existing_returns_200() -> Result<()> {
    let tenant = Uuid::now_v7();
    let router = build_router(tenant, true).await?;
    let (id, _etag) = create_role(&router, "Auditor", tenant).await;
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/rbac/v1/role-definitions/{id}"))
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(value["id"].as_str().expect("id"), id);
    assert_eq!(value["name"], "Auditor");
    Ok(())
}

#[tokio::test]
async fn get_nonexistent_returns_404() -> Result<()> {
    let tenant = Uuid::now_v7();
    let router = build_router(tenant, true).await?;
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/rbac/v1/role-definitions/{}", Uuid::now_v7()))
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::NOT_FOUND, "not_found").await;
    Ok(())
}

#[tokio::test]
async fn list_returns_created_custom_role() -> Result<()> {
    let tenant = Uuid::now_v7();
    // Unrestricted readable scopes so the custom row is visible in the page.
    let router = build_harness(tenant, policy_reading_everything())
        .await?
        .router;
    let (id, _etag) = create_role(&router, "Lister", tenant).await;

    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/rbac/v1/role-definitions")
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let page: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let items = page["items"].as_array().expect("items array");
    assert!(
        items
            .iter()
            .any(|it| it["id"].as_str() == Some(id.as_str())),
        "list MUST include the created custom role; page={page}"
    );
    Ok(())
}

#[tokio::test]
async fn patch_description_with_if_match_returns_200() -> Result<()> {
    let tenant = Uuid::now_v7();
    let router = build_router(tenant, true).await?;
    let (id, etag) = create_role(&router, "Auditor", tenant).await;

    let response = router
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/rbac/v1/role-definitions/{id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, etag)
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({ "description": "updated" }))
                        .expect("json"),
                ))
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(value["description"], "updated");
    Ok(())
}

#[tokio::test]
async fn patch_immutable_field_returns_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let router = build_router(tenant, true).await?;
    let (id, etag) = create_role(&router, "Auditor", tenant).await;

    // `owner_tenant_id` is immutable; PATCHing it MUST be rejected.
    let response = router
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/rbac/v1/role-definitions/{id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, etag)
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "owner_tenant_id": Uuid::now_v7()
                    }))
                    .expect("json"),
                ))
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    Ok(())
}

#[tokio::test]
async fn patch_missing_if_match_returns_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let router = build_router(tenant, true).await?;
    let (id, _etag) = create_role(&router, "Auditor", tenant).await;

    let response = router
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/rbac/v1/role-definitions/{id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({ "description": "x" })).expect("json"),
                ))
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::BAD_REQUEST, "failed_precondition").await;
    Ok(())
}

#[tokio::test]
async fn patch_stale_if_match_returns_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let router = build_router(tenant, true).await?;
    let (id, etag1) = create_role(&router, "Auditor", tenant).await;

    // First PATCH with the fresh etag succeeds and bumps `updated_at`.
    let ok = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/rbac/v1/role-definitions/{id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, etag1.clone())
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({ "description": "first" }))
                        .expect("json"),
                ))
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(ok.status(), StatusCode::OK);

    // Re-using the now-stale etag1 → optimistic-concurrency stale (400).
    let response = router
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/rbac/v1/role-definitions/{id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, etag1)
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({ "description": "second" }))
                        .expect("json"),
                ))
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::BAD_REQUEST, "failed_precondition").await;
    Ok(())
}

#[tokio::test]
async fn delete_existing_returns_204() -> Result<()> {
    let tenant = Uuid::now_v7();
    let router = build_router(tenant, true).await?;
    let (id, etag) = create_role(&router, "Auditor", tenant).await;

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/rbac/v1/role-definitions/{id}"))
                .header(header::IF_MATCH, etag)
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Now gone.
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/rbac/v1/role-definitions/{id}"))
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn delete_nonexistent_returns_404() -> Result<()> {
    let tenant = Uuid::now_v7();
    let router = build_router(tenant, true).await?;
    // Valid-format (timestamp:uuid) but for a row that does not exist.
    let etag = format!("\"1970-01-01T00:00:00.000000Z:{}\"", Uuid::now_v7());
    let response = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/rbac/v1/role-definitions/{}", Uuid::now_v7()))
                .header(header::IF_MATCH, etag)
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::NOT_FOUND, "not_found").await;
    Ok(())
}

#[tokio::test]
async fn duplicate_name_returns_409() -> Result<()> {
    let tenant = Uuid::now_v7();
    let router = build_router(tenant, true).await?;
    let _ = create_role(&router, "Auditor", tenant).await;

    let body = serde_json::to_vec(&create_body("Auditor", tenant)).expect("json");
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-definitions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::CONFLICT, "already_exists").await;
    Ok(())
}

#[tokio::test]
async fn policy_deny_returns_403() -> Result<()> {
    let tenant = Uuid::now_v7();
    let router = build_router(tenant, false).await?;
    let body = serde_json::to_vec(&create_body("Auditor", tenant)).expect("json");
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-definitions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::FORBIDDEN, "permission_denied").await;
    Ok(())
}

#[tokio::test]
async fn malformed_json_returns_problem_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let router = build_router(tenant, true).await?;
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-definitions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{invalid"))
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    Ok(())
}

// ---------------------------------------------------------------------------
// `assignment_count` on list + point read
// ---------------------------------------------------------------------------

/// Seed `count` role assignments against `role` at the tenant scope.
/// Principal ids are distinct so the `uq_assignment` tuple stays unique.
async fn seed_assignments(
    conn: &toolkit_db::secure::DbConn<'_>,
    assignments: &Arc<role_assignment_repo::RoleAssignmentRepository>,
    role: Uuid,
    tenant: Uuid,
    count: usize,
) {
    for i in 0..count {
        assignments
            .create(
                conn,
                NewRoleAssignment {
                    role_definition_id: role,
                    principal_id: format!("user-{i}"),
                    principal_type: rbac_sdk::models::PrincipalType::User,
                    scope: rbac_sdk::models::Scope::tenant(tenant),
                    created_by: "seeder".to_owned(),
                    created_by_type: None,
                    created_by_tenant_id: None,
                },
            )
            .await
            .expect("seed assignment");
    }
}

/// GET the resource at `uri` and return the parsed JSON body, asserting 200.
async fn get_json(router: &Router, uri: &str) -> serde_json::Value {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK, "GET {uri} must succeed");
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

/// The list carries a per-row count, and a role with no assignments reports
/// `0` rather than omitting the key: the caller CAN see assignments here, so
/// zero is a real answer about the role, not a statement about the caller.
#[tokio::test]
async fn list_carries_assignment_count_per_role() -> Result<()> {
    let tenant = Uuid::now_v7();
    let harness = build_harness(tenant, policy_reading_everything()).await?;
    let (used_id, _) = create_role(&harness.router, "Used", tenant).await;
    let (unused_id, _) = create_role(&harness.router, "Unused", tenant).await;
    seed_assignments(
        &harness.provider.conn()?,
        &harness.assignments,
        used_id.parse().expect("uuid"),
        tenant,
        3,
    )
    .await;

    let page = get_json(&harness.router, "/rbac/v1/role-definitions").await;
    let items = page["items"].as_array().expect("items array");
    let used = items
        .iter()
        .find(|it| it["id"].as_str() == Some(used_id.as_str()))
        .expect("used role on the page");
    let unused = items
        .iter()
        .find(|it| it["id"].as_str() == Some(unused_id.as_str()))
        .expect("unused role on the page");
    assert_eq!(
        used["assignment_count"], 3,
        "the role with three grants must report three; page={page}"
    );
    assert_eq!(
        unused["assignment_count"], 0,
        "a visible-but-unused role reports 0, never an absent key; page={page}"
    );
    Ok(())
}

/// The point read carries the same number the list does — one endpoint's
/// count must not disagree with the other's for the same row.
#[tokio::test]
async fn point_read_carries_assignment_count() -> Result<()> {
    let tenant = Uuid::now_v7();
    let harness = build_harness(tenant, policy_reading_everything()).await?;
    let (id, _) = create_role(&harness.router, "Counted", tenant).await;
    seed_assignments(
        &harness.provider.conn()?,
        &harness.assignments,
        id.parse().expect("uuid"),
        tenant,
        2,
    )
    .await;

    let body = get_json(&harness.router, &format!("/rbac/v1/role-definitions/{id}")).await;
    assert_eq!(body["assignment_count"], 2, "body={body}");
    Ok(())
}

/// A caller who can read role definitions but **no** role assignments gets
/// the rows with the key omitted — not `0`. A zero would be a fact about
/// their own permissions rendered in the UI as "this role is unused".
#[tokio::test]
async fn assignment_count_absent_when_caller_reads_no_assignments() -> Result<()> {
    let tenant = Uuid::now_v7();
    // Unrestricted on role definitions; nothing matches for role
    // assignments, and the mock's closed-posture default is
    // `ReadableScopes::None`.
    let policy = Arc::new(MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred {
            target_type: Some("gts.cf.core.rbac.role_definition.v1~".to_owned()),
            ..ReadableScopesPred::default()
        },
        ReadableScopes::Unrestricted,
    )]));
    let harness = build_harness(tenant, policy).await?;
    let (id, _) = create_role(&harness.router, "Opaque", tenant).await;
    // Real grants exist — the point is that this caller must not learn so.
    seed_assignments(
        &harness.provider.conn()?,
        &harness.assignments,
        id.parse().expect("uuid"),
        tenant,
        4,
    )
    .await;

    let page = get_json(&harness.router, "/rbac/v1/role-definitions").await;
    let row = page["items"]
        .as_array()
        .expect("items array")
        .iter()
        .find(|it| it["id"].as_str() == Some(id.as_str()))
        .expect("row on the page");
    assert!(
        row.get("assignment_count").is_none(),
        "a caller with no assignment-read visibility must see no count at all \
         (a 0 would leak as \"unused\"); row={row}"
    );

    let body = get_json(&harness.router, &format!("/rbac/v1/role-definitions/{id}")).await;
    assert!(
        body.get("assignment_count").is_none(),
        "the point read must omit it for the same reason; body={body}"
    );
    Ok(())
}

/// `assignment_count` is computed, not a column, so it is absent from the
/// filter-field enum and a `$filter` naming it is the standard unknown-field
/// rejection — the same rule the display names follow.
#[tokio::test]
async fn filter_on_assignment_count_is_rejected() -> Result<()> {
    let tenant = Uuid::now_v7();
    let harness = build_harness(tenant, policy_reading_everything()).await?;
    let response = harness
        .router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/rbac/v1/role-definitions?%24filter=assignment_count%20eq%201")
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    Ok(())
}

/// Same for `$orderby`: ordering by a number computed after the page was
/// selected would silently break the keyset contract.
#[tokio::test]
async fn orderby_on_assignment_count_is_rejected() -> Result<()> {
    let tenant = Uuid::now_v7();
    let harness = build_harness(tenant, policy_reading_everything()).await?;
    let response = harness
        .router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/rbac/v1/role-definitions?%24orderby=assignment_count%20desc")
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    Ok(())
}

/// The write paths perform no count, so the 201 body omits the key rather
/// than claiming a zero it never measured.
#[tokio::test]
async fn create_response_omits_assignment_count() -> Result<()> {
    let tenant = Uuid::now_v7();
    let harness = build_harness(tenant, policy_reading_everything()).await?;
    let body = serde_json::to_vec(&create_body("Fresh", tenant)).expect("json");
    let response = harness
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-definitions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let created: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(
        created.get("assignment_count").is_none(),
        "a create response must not carry a count it never took; body={created}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// `GET /rbac/v1/role-definitions/summary`
// ---------------------------------------------------------------------------

/// The summary buckets what the caller can see and derives `total` from the
/// two parts. `built_in` is `0` here because the SQLite fixture runs the
/// migrations without the built-in seeder; the built-in / custom split
/// itself is pinned at repo level by
/// `postgres_role_definition_repo::count_by_type_splits_builtin_from_custom`.
#[tokio::test]
async fn summary_returns_builtin_and_custom_buckets() -> Result<()> {
    let tenant = Uuid::now_v7();
    let harness = build_harness(tenant, policy_reading_everything()).await?;
    let _ = create_role(&harness.router, "First", tenant).await;
    let _ = create_role(&harness.router, "Second", tenant).await;

    let body = get_json(&harness.router, "/rbac/v1/role-definitions/summary").await;
    assert_eq!(body["custom"], 2, "both custom roles counted; body={body}");
    assert_eq!(body["built_in"], 0, "no built-ins seeded; body={body}");
    assert_eq!(
        body["total"].as_u64().expect("total"),
        body["built_in"].as_u64().expect("built_in") + body["custom"].as_u64().expect("custom"),
        "total is derived from the two buckets, never queried separately"
    );
    Ok(())
}

// The static `summary` segment must not be swallowed by the sibling
// `GET /role-definitions/{id}` route, which extracts a `Uuid`. Were the
// parameter route to win, `summary` would fail to parse and the request
// would answer 400 — so the assertion is on the body SHAPE (counts, no
// `id`), which is unambiguous either way.
#[tokio::test]
async fn summary_path_is_not_shadowed_by_get_by_id() -> Result<()> {
    let tenant = Uuid::now_v7();
    let harness = build_harness(tenant, policy_reading_everything()).await?;
    let _ = create_role(&harness.router, "Solo", tenant).await;

    let body = get_json(&harness.router, "/rbac/v1/role-definitions/summary").await;
    assert!(
        body.get("id").is_none(),
        "the summary route must win over GET /role-definitions/{{id}} - got a \
         single-role body: {body}"
    );
    assert_eq!(body["custom"], 1, "body={body}");
    assert_eq!(body["total"], 1, "body={body}");
    Ok(())
}

// The counterpart to the shadowing test above: static-segment matching is
// case-sensitive, so `…/role-definitions/SUMMARY` falls through to
// `GET /{id}` and earns that route's malformed-UUID rejection rather than
// the counts body. This is the assertion that would catch a routing library
// switching to case-insensitive matching, which would make the reserved word
// wider than intended.
#[tokio::test]
async fn summary_route_matches_the_reserved_word_case_sensitively() -> Result<()> {
    let tenant = Uuid::now_v7();
    let harness = build_harness(tenant, policy_reading_everything()).await?;
    let response = harness
        .router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/rbac/v1/role-definitions/SUMMARY")
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    Ok(())
}

// Registering a static sibling must not change what the by-id route does
// with a non-UUID segment: it stays that route's canonical 400.
#[tokio::test]
async fn get_by_id_with_non_uuid_still_returns_the_same_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let harness = build_harness(tenant, policy_reading_everything()).await?;
    let response = harness
        .router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/rbac/v1/role-definitions/not-a-uuid")
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    Ok(())
}

// ---------------------------------------------------------------------------
// `GET /rbac/v1/role-definitions/summary` — tenant isolation
// ---------------------------------------------------------------------------
//
// Every summary test above runs with `policy_reading_everything()`, which
// answers `Unrestricted` for every resource type. That makes them blind to
// the endpoint's one security property: a regression widening the projection
// to `RoleDefinitionVisibility::All`, or dropping the policy enforcer from
// the path entirely, would publish the platform-wide custom-role count to
// every tenant admin and still pass all three. The fixture below is a
// genuinely restricted caller, so it does not.

/// Router + role-definition repo over one provider, wired for a *restricted*
/// caller: the router carries a non-root `SecurityContext` bound to
/// `caller_tenant`, and `readable_scopes` grants read only inside that
/// tenant's subtree. `enforce` stays `allow_all` so nothing here is masked by
/// a blanket denial — the only narrowing under test is the readable-scope
/// projection the summary derives.
///
/// The two tenants are seeded as disjoint subtrees so neither is an ancestor
/// of the other; "another tenant's rows" must mean rows genuinely outside the
/// caller's reach.
async fn build_tenant_scoped_harness(
    caller_tenant: Uuid,
    other_tenant: Uuid,
) -> Result<(
    Router,
    Arc<role_definition_repo::RoleDefinitionRepository>,
    toolkit_db::DBProvider<toolkit_db::DbError>,
)> {
    let provider = common::fresh_sqlite_provider().await?;
    let repo = Arc::new(role_definition_repo::RoleDefinitionRepository);
    let assignments = Arc::new(role_assignment_repo::RoleAssignmentRepository);
    let tenant_resolver = Arc::new(fakes::FakeTenantResolverClient::with_disjoint_subtrees(&[
        &[caller_tenant],
        &[other_tenant],
    ])) as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg = Arc::new(NoopRbacRgRead) as Arc<dyn rbac::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, rg));
    let target_type_validator: Arc<dyn TargetTypeValidator> =
        Arc::new(AcceptAllTargetTypeValidator::new());
    let policy = Arc::new(MockPolicyEnforcer::allow_all().with_readable_scopes(vec![(
        ReadableScopesPred::default(),
        ReadableScopes::Subtrees(vec![format!("/tenants/{caller_tenant}")]),
    )]));
    let service = Arc::new(RoleDefinitionService::new(
        provider.clone(),
        Arc::clone(&repo),
        assignments,
        policy,
        scope_validator,
        target_type_validator,
    ));
    let state = Arc::new(ApiState { service });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    let app = common::with_non_root_security_context(router(state, &openapi), caller_tenant);
    Ok((app, repo, provider))
}

/// Seed one custom role owned by `tenant` straight through the repository.
///
/// Not over HTTP: a tenant-bound caller cannot legally create a role
/// definition owned by a different tenant (`OwnerTenantMismatch`), and having
/// the other tenant's rows physically present is the whole point of the
/// fixture — the summary must exclude them by projection, not by their
/// absence from the table.
async fn seed_custom_role(
    conn: &toolkit_db::secure::DbConn<'_>,
    repo: &Arc<role_definition_repo::RoleDefinitionRepository>,
    name: &str,
    tenant: Uuid,
) -> Result<()> {
    repo.create(
        conn,
        rbac::domain::role_definition_repo::NewRoleDefinition {
            id: Uuid::now_v7(),
            name: name.to_owned(),
            description: None,
            permissions: vec![rbac_sdk::models::PermissionRule::new(
                "read",
                "gts.cf.resources.compute.vm.v1~",
            )],
            not_permissions: Vec::new(),
            assignable_scopes: vec![rbac_sdk::models::Scope::tenant(tenant)],
            owner_tenant_id: tenant,
            created_by: "test".to_owned(),
        },
    )
    .await?;
    Ok(())
}

/// A tenant-scoped admin's summary counts their own tenant's custom roles and
/// nothing else. Both tenants have a custom role in the table; only one is
/// the caller's. `custom == 2` here would mean the projection was widened to
/// `RoleDefinitionVisibility::All` — the platform-wide count leaking to a
/// tenant admin.
#[tokio::test]
async fn summary_for_tenant_scoped_caller_excludes_other_tenants_customs() -> Result<()> {
    let mine = Uuid::now_v7();
    let theirs = Uuid::now_v7();
    let (router, repo, provider) = build_tenant_scoped_harness(mine, theirs).await?;
    let conn = provider.conn()?;
    seed_custom_role(&conn, &repo, "Mine", mine).await?;
    seed_custom_role(&conn, &repo, "Theirs", theirs).await?;

    let body = get_json(&router, "/rbac/v1/role-definitions/summary").await;

    assert_eq!(
        body["custom"], 1,
        "only the caller's own tenant's custom role may be counted - another \
         tenant's custom role leaked into the summary; body={body}"
    );
    // `built_in` is 0 because the SQLite fixture runs the migrations without
    // the built-in seeder; that built-ins stay counted for a tenant-scoped
    // caller is pinned at service level by
    // `summary_counts_only_the_callers_own_tenant_customs`.
    assert_eq!(body["built_in"], 0, "no built-ins seeded; body={body}");
    assert_eq!(body["total"], 1, "body={body}");
    Ok(())
}

/// The same caller's list agrees with their summary: the summary's whole job
/// is to describe the rows the list would page, so a projection that drifted
/// between the two would make the number unreproducible by the caller.
#[tokio::test]
async fn summary_agrees_with_the_list_for_a_tenant_scoped_caller() -> Result<()> {
    let mine = Uuid::now_v7();
    let theirs = Uuid::now_v7();
    let (router, repo, provider) = build_tenant_scoped_harness(mine, theirs).await?;
    let conn = provider.conn()?;
    seed_custom_role(&conn, &repo, "Mine", mine).await?;
    seed_custom_role(&conn, &repo, "Theirs", theirs).await?;

    let page = get_json(&router, "/rbac/v1/role-definitions").await;
    let items = page["items"].as_array().expect("items array");
    let summary = get_json(&router, "/rbac/v1/role-definitions/summary").await;

    assert_eq!(
        u64::try_from(items.len()).expect("page size fits u64"),
        summary["total"].as_u64().expect("total"),
        "the summary must count exactly the rows the list pages; \
         page={page} summary={summary}"
    );
    assert!(
        items.iter().all(|it| it["name"] != "Theirs"),
        "another tenant's custom role must not appear in the page either; page={page}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// `assignable_scopes` with MORE THAN ONE entry
// ---------------------------------------------------------------------------
//
// Every other test in this file (and in the Postgres suite) creates roles
// with a single-element `assignable_scopes`, so the per-entry validation in
// `RoleDefinitionService::create` — and in particular its owner-subtree
// rule — is exercised with a list only here. The tests below pin both
// sides of that rule:
//
//   * a list whose entries live in two UNRELATED tenants is rejected: an
//     entry outside the owner tenant's subtree is not something the role
//     may be assignable in;
//   * a list of several scopes that all sit inside the owner tenant (the
//     tenant itself plus two of its resource groups) is accepted and stored
//     in order;
//   * a list naming two CHILD tenants of the owner is accepted — children
//     are inside the owner's subtree, which is the containment rule the
//     design states.

/// Router whose scope validator knows the tenant hierarchy described by
/// `tenant_branches` — each branch is a root-to-leaf chain, and branches
/// sharing a first element share that parent — plus every
/// `(group_id, owner_tenant_id)` pair in `groups`.
///
/// `build_harness` hardwires a one-tenant chain and `NoopRbacRgRead`, which
/// cannot express any half of a multi-scope role: a second tenant, a
/// parent/child pair, or a resource group that actually resolves.
async fn build_multi_scope_router(
    tenant_branches: &[&[Uuid]],
    groups: &[(Uuid, Uuid)],
) -> Result<Router> {
    let provider = common::fresh_sqlite_provider().await?;
    let repo = Arc::new(role_definition_repo::RoleDefinitionRepository);
    let assignments = Arc::new(role_assignment_repo::RoleAssignmentRepository);
    let tenant_resolver = Arc::new(fakes::FakeTenantResolverClient::with_disjoint_subtrees(
        tenant_branches,
    )) as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let mut rg_fake = fakes::FakeRbacRgRead::default();
    for (group_id, owner_tenant_id) in groups {
        rg_fake = rg_fake.with_group(*group_id, *owner_tenant_id);
    }
    let rg = Arc::new(rg_fake) as Arc<dyn rbac::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, rg));
    let target_type_validator: Arc<dyn TargetTypeValidator> =
        Arc::new(AcceptAllTargetTypeValidator::new());
    let service = Arc::new(RoleDefinitionService::new(
        provider,
        repo,
        assignments,
        Arc::new(MockPolicyEnforcer::allow_all()),
        scope_validator,
        target_type_validator,
    ));
    let state = Arc::new(ApiState { service });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    Ok(with_test_security_context(router(state, &openapi)))
}

/// `create_body` with an explicit multi-entry `assignable_scopes`.
fn create_body_with_scopes(name: &str, owner_tenant: Uuid, scopes: &[String]) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": "multi-scope",
        "permissions": [
            { "operation": "read", "target_type": "gts.cf.resources.compute.vm.v1~" }
        ],
        "not_permissions": [],
        "assignable_scopes": scopes,
        "owner_tenant_id": owner_tenant,
    })
}

/// POST `body` to the role-definition collection and hand back the raw
/// response, so each caller can assert its own status and problem shape.
async fn post_role(router: &Router, body: &serde_json::Value) -> axum::response::Response {
    let bytes = serde_json::to_vec(body).expect("json");
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-definitions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(bytes))
                .expect("build req"),
        )
        .await
        .expect("send")
}

/// Two existing but UNRELATED tenants, one role: naming both of them in
/// `assignable_scopes` MUST be rejected. Every entry has to sit inside the
/// owner tenant's subtree, and the second entry belongs to a tenant that is
/// neither the owner nor a descendant of it, so validation fails before any
/// row is written. Accepting it would let a tenant admin mint a role
/// assignable inside somebody else's tenant.
#[tokio::test]
async fn create_with_scopes_in_two_unrelated_tenants_is_rejected() -> Result<()> {
    let owner = Uuid::now_v7();
    let other = Uuid::now_v7();
    // Both tenants exist, so the failure cannot be "scope not found".
    let router = build_multi_scope_router(&[&[owner], &[other]], &[]).await?;

    let response = post_role(
        &router,
        &create_body_with_scopes(
            "TwoTenants",
            owner,
            &[format!("/tenants/{owner}"), format!("/tenants/{other}")],
        ),
    )
    .await;

    let problem =
        common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    assert!(
        problem.to_string().contains("not within owner tenant"),
        "the rejection must name the owner-subtree rule (and the offending \
         index), not some other validation failure; problem={problem}"
    );
    Ok(())
}

/// Several scopes that all sit inside the owner tenant — the tenant itself
/// plus two of its resource groups — are accepted, stored, and echoed back
/// in the order they were sent.
#[tokio::test]
async fn create_accepts_several_scopes_inside_the_owner_tenant() -> Result<()> {
    let owner = Uuid::now_v7();
    let rg_a = Uuid::now_v7();
    let rg_b = Uuid::now_v7();
    let router = build_multi_scope_router(&[&[owner]], &[(rg_a, owner), (rg_b, owner)]).await?;
    let scopes = vec![
        format!("/tenants/{owner}"),
        format!("/tenants/{owner}/resourceGroups/{rg_a}"),
        format!("/tenants/{owner}/resourceGroups/{rg_b}"),
    ];

    let response = post_role(
        &router,
        &create_body_with_scopes("MultiScope", owner, &scopes),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        value["assignable_scopes"],
        serde_json::json!(scopes),
        "all three scopes must round-trip, in order; body={value}"
    );

    // And they survive the read path, not just the create echo.
    let fetched = get_json(
        &router,
        &format!(
            "/rbac/v1/role-definitions/{}",
            value["id"].as_str().expect("id field")
        ),
    )
    .await;
    assert_eq!(
        fetched["assignable_scopes"],
        serde_json::json!(scopes),
        "the stored row must carry the same list; body={fetched}"
    );
    Ok(())
}

/// A role owned by a PARENT tenant IS assignable in two of its CHILD
/// tenants. This is the shape an operator reaches for first — "one role,
/// usable in these two sub-tenants" — and the design's containment rule
/// admits it: every entry must stay within the owner tenant's subtree, and
/// a child is inside that subtree.
///
/// The scopes name neither the owner itself nor any resource group of it,
/// so nothing here can pass on the structural same-tenant shortcut — the
/// tenant hierarchy has to be consulted for both entries.
#[tokio::test]
async fn create_with_scopes_in_two_child_tenants_is_accepted() -> Result<()> {
    let parent = Uuid::now_v7();
    let child_a = Uuid::now_v7();
    let child_b = Uuid::now_v7();
    // Two branches sharing `parent` as their root: both children hang off it.
    let router = build_multi_scope_router(&[&[parent, child_a], &[parent, child_b]], &[]).await?;
    let scopes = [format!("/tenants/{child_a}"), format!("/tenants/{child_b}")];

    let response = post_role(
        &router,
        &create_body_with_scopes("TwoChildren", parent, &scopes),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        value["assignable_scopes"],
        serde_json::json!(scopes),
        "both child-tenant scopes must round-trip; body={value}"
    );
    Ok(())
}

/// The mirror of the case above: a tenant OUTSIDE the owner's subtree stays
/// rejected even when it exists. Without this pin, a fix that admitted every
/// resolvable tenant would pass the child-tenant test and quietly drop the
/// containment rule altogether.
#[tokio::test]
async fn create_with_a_scope_in_an_unrelated_tenant_is_still_rejected() -> Result<()> {
    let parent = Uuid::now_v7();
    let child = Uuid::now_v7();
    let stranger = Uuid::now_v7();
    let router = build_multi_scope_router(&[&[parent, child], &[stranger]], &[]).await?;

    let response = post_role(
        &router,
        &create_body_with_scopes(
            "ChildPlusStranger",
            parent,
            &[format!("/tenants/{child}"), format!("/tenants/{stranger}")],
        ),
    )
    .await;

    let problem =
        common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    let text = problem.to_string();
    assert!(
        text.contains("not within owner tenant"),
        "the unrelated tenant must be reported against the owner-subtree rule; \
         problem={problem}"
    );
    assert!(
        text.contains("assignable_scopes[1]"),
        "the child entry at index 0 is legal now, so the report must point at \
         index 1; problem={problem}"
    );
    Ok(())
}

/// A root-scoped caller may name any `owner_tenant_id` in the body, and
/// nothing validates that the tenant exists before it is used as the
/// root of the containment check. When it does not exist, that is a bad
/// request about the role — it must not surface as a 500.
///
/// The assignable scope itself is a tenant that DOES exist, so the
/// rejection cannot be blamed on the scope: `validate_scope_exists`
/// passes, and only the owner endpoint is missing when the hierarchy is
/// consulted.
#[tokio::test]
async fn create_with_an_owner_tenant_that_does_not_exist_is_rejected() -> Result<()> {
    let real = Uuid::now_v7();
    let ghost = Uuid::now_v7(); // never seeded in the tenant hierarchy
    let router = build_multi_scope_router(&[&[real]], &[]).await?;

    let response = post_role(
        &router,
        &create_body_with_scopes("GhostOwner", ghost, &[format!("/tenants/{real}")]),
    )
    .await;

    let problem =
        common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    assert!(
        problem.to_string().contains("owner tenant"),
        "the rejection must name the unresolvable owner tenant, not an \
         internal failure; problem={problem}"
    );
    Ok(())
}

/// The owner-subtree rule gates PATCH as well as create, and PATCH is the
/// path an operator actually reaches for when a sub-tenant is added
/// later. Both halves are pinned here: a child tenant is accepted, and an
/// unrelated tenant in the same list is still refused with the index that
/// names it.
#[tokio::test]
async fn patch_assignable_scopes_honours_the_owner_subtree() -> Result<()> {
    let parent = Uuid::now_v7();
    let child = Uuid::now_v7();
    let stranger = Uuid::now_v7();
    let router = build_multi_scope_router(&[&[parent, child], &[stranger]], &[]).await?;

    let created = post_role(
        &router,
        &create_body_with_scopes("PatchSubtree", parent, &[format!("/tenants/{parent}")]),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let etag = created
        .headers()
        .get(header::ETAG)
        .expect("etag header")
        .to_str()
        .expect("etag is ascii")
        .to_owned();
    let bytes = to_bytes(created.into_body(), 1_000_000)
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let id = value["id"].as_str().expect("id field").to_owned();

    let patch = |scopes: Vec<String>, etag: String| {
        let router = router.clone();
        let id = id.clone();
        async move {
            router
                .oneshot(
                    Request::builder()
                        .method("PATCH")
                        .uri(format!("/rbac/v1/role-definitions/{id}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::IF_MATCH, etag)
                        .body(Body::from(
                            serde_json::to_vec(&serde_json::json!({ "assignable_scopes": scopes }))
                                .expect("json"),
                        ))
                        .expect("build req"),
                )
                .await
                .expect("send")
        }
    };

    // A child tenant is inside the owner's subtree, so the patch lands.
    let response = patch(vec![format!("/tenants/{child}")], etag).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a child tenant must be patchable into assignable_scopes"
    );
    // The row's ETag moved with the write; take the fresh one from the
    // response header so the second patch is not rejected as stale.
    let next_etag = response
        .headers()
        .get(header::ETAG)
        .expect("patch response carries an etag")
        .to_str()
        .expect("etag is ascii")
        .to_owned();
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        value["assignable_scopes"],
        serde_json::json!([format!("/tenants/{child}")])
    );

    // A tenant outside the subtree is still refused, named by its index.
    let response = patch(
        vec![format!("/tenants/{child}"), format!("/tenants/{stranger}")],
        next_etag,
    )
    .await;
    let problem =
        common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    let text = problem.to_string();
    assert!(
        text.contains("not within owner tenant") && text.contains("assignable_scopes[1]"),
        "PATCH must apply the same owner-subtree rule, and point at the \
         offending index; problem={problem}"
    );
    Ok(())
}

/// `assignable_scopes` is bounded. Every entry costs the writer two
/// tenant-resolver round-trips, and costs every later assignment create
/// one more while the envelope is searched, so the list length is
/// caller-controlled load on a shared component.
///
/// The boundary is asserted from both sides so a future change to the
/// limit cannot pass by loosening only one of them.
#[tokio::test]
async fn create_rejects_more_assignable_scopes_than_the_limit() -> Result<()> {
    let owner = Uuid::now_v7();
    // Duplicates are legal, so the list needs no supporting tenants —
    // which is precisely why length alone has to be bounded.
    let at_limit: Vec<String> = std::iter::repeat_n(format!("/tenants/{owner}"), 10).collect();
    let over_limit: Vec<String> = std::iter::repeat_n(format!("/tenants/{owner}"), 11).collect();
    let router = build_multi_scope_router(&[&[owner]], &[]).await?;

    let response = post_role(
        &router,
        &create_body_with_scopes("AtLimit", owner, &at_limit),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "ten entries is inside the limit and must still be accepted"
    );

    let response = post_role(
        &router,
        &create_body_with_scopes("OverLimit", owner, &over_limit),
    )
    .await;
    let problem =
        common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    assert!(
        problem.to_string().contains("at most 10 scopes"),
        "the rejection must name the limit; problem={problem}"
    );
    Ok(())
}

/// PATCH is gated by the same bound as create — otherwise the limit is
/// one request away from being bypassed on any existing role.
#[tokio::test]
async fn patch_rejects_more_assignable_scopes_than_the_limit() -> Result<()> {
    let owner = Uuid::now_v7();
    let router = build_multi_scope_router(&[&[owner]], &[]).await?;
    let created = post_role(
        &router,
        &create_body_with_scopes("PatchLimit", owner, &[format!("/tenants/{owner}")]),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let etag = created
        .headers()
        .get(header::ETAG)
        .expect("etag header")
        .to_str()
        .expect("etag is ascii")
        .to_owned();
    let bytes = to_bytes(created.into_body(), 1_000_000)
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let id = value["id"].as_str().expect("id field").to_owned();

    let over_limit: Vec<String> = std::iter::repeat_n(format!("/tenants/{owner}"), 11).collect();
    let response = router
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/rbac/v1/role-definitions/{id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, etag)
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "assignable_scopes": over_limit
                    }))
                    .expect("json"),
                ))
                .expect("build req"),
        )
        .await
        .expect("send");

    let problem =
        common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    assert!(
        problem.to_string().contains("at most 10 scopes"),
        "PATCH must apply the same bound as create; problem={problem}"
    );
    Ok(())
}

/// A tenant-owned role can never be assignable at the root scope.
///
/// `/` sits strictly above every tenant, so it is outside any owner
/// tenant's subtree. Admitting it would make a custom role assignable
/// anywhere on the platform, which is the one thing the containment rule
/// exists to prevent — v1 has no global custom roles. The e2e suite pins
/// this against a live server; pinned here too so the rule is covered by
/// the default `cargo test` run and not only by a deployed stand.
#[tokio::test]
async fn create_with_the_root_scope_is_rejected_for_a_tenant_owned_role() -> Result<()> {
    let owner = Uuid::now_v7();
    let router = build_multi_scope_router(&[&[owner]], &[]).await?;

    let response = post_role(
        &router,
        &create_body_with_scopes("RootScoped", owner, &["/".to_owned()]),
    )
    .await;

    let problem =
        common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    assert!(
        problem.to_string().contains("not within owner tenant"),
        "the root scope must be refused against the owner-subtree rule; problem={problem}"
    );
    Ok(())
}
