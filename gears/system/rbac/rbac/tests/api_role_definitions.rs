//! API-level tests for `/rbac/v1/role-definitions` end-to-end against a
//! freshly-migrated PostgreSQL testcontainer, with a mock enforcer +
//! fake tenant resolver + fake rg. Every test runs in its own container.

#![cfg(test)]
#![allow(clippy::expect_used, clippy::doc_markdown)]

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use tower::ServiceExt;
use uuid::Uuid;

use rbac::api::rest::role_definitions::{ApiState, router};
use rbac::domain::policy_enforcer::MockPolicyEnforcer;
use rbac::domain::role_definition::RoleDefinitionService;
use rbac::domain::scope_validator::ScopeValidator;
use rbac::infra::storage::{role_assignment_repo, role_definition_repo};

mod common;
use common::scope_fakes as fakes;
use common::{with_non_root_security_context, with_test_security_context};

/// Spin up a fresh PostgreSQL testcontainer and return a SeaORM-backed
/// `dyn RoleDefinitionRepository` plus the fixture (kept alive by the caller).
async fn fresh_repo() -> Result<(
    Arc<role_definition_repo::RoleDefinitionRepository>,
    DBProvider<DbError>,
    common::PostgresUnderTest,
)> {
    let fixture = common::bring_up_migrated_postgres().await?;
    let db = connect_db(&fixture.url, ConnectOpts::default()).await?;
    let provider: DBProvider<DbError> = DBProvider::new(db);
    let repo = Arc::new(role_definition_repo::RoleDefinitionRepository);
    Ok((repo, provider, fixture))
}

/// Build a router wired to a freshly-created Postgres testcontainer.
async fn build_router(
    tenant: Uuid,
    allow: bool,
) -> Result<(
    Router,
    Arc<role_definition_repo::RoleDefinitionRepository>,
    common::PostgresUnderTest,
)> {
    let (repo, provider, fixture) = fresh_repo().await?;
    let router = build_router_with_repo(provider, tenant, allow, &repo);
    Ok((router, repo, fixture))
}

/// Construct a router around an existing repo handle so a single test
/// can build two routers sharing the same Postgres rows.
fn build_router_with_repo(
    provider: DBProvider<DbError>,
    tenant: Uuid,
    allow: bool,
    repo: &Arc<role_definition_repo::RoleDefinitionRepository>,
) -> Router {
    let policy: Arc<MockPolicyEnforcer> = if allow {
        Arc::new(MockPolicyEnforcer::allow_all())
    } else {
        Arc::new(MockPolicyEnforcer::deny_all())
    };
    let tenant_resolver = Arc::new(fakes::FakeTenantResolverClient::with_chain(&[tenant]))
        as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    // Tests use tenant-only scopes, so a no-op `RbacRgRead` is sufficient.
    let rg = Arc::new(NoopRbacRgRead) as Arc<dyn rbac::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, rg));
    let target_type_validator: Arc<dyn rbac::domain::target_type_validator::TargetTypeValidator> =
        Arc::new(rbac::domain::target_type_validator::AcceptAllTargetTypeValidator::new());
    let service = Arc::new(RoleDefinitionService::new(
        provider,
        Arc::clone(repo),
        // These tests assert on role-definition behaviour only; the
        // per-role assignment count reads an empty store.
        Arc::new(role_assignment_repo::RoleAssignmentRepository),
        policy,
        scope_validator,
        Arc::clone(&target_type_validator),
    ));
    let state = Arc::new(ApiState { service });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    with_test_security_context(router(state, &openapi))
}

/// Like [`build_router`] but wraps the router with a **non-root**,
/// tenant-bound `SecurityContext` so the `is_first_party_root ==
/// false` authorization branch is exercised at the wire level. The
/// caller is bound to `caller_tenant`.
async fn build_non_root_router(
    caller_tenant: Uuid,
) -> Result<(
    Router,
    Arc<role_definition_repo::RoleDefinitionRepository>,
    common::PostgresUnderTest,
)> {
    let (repo, provider, fixture) = fresh_repo().await?;
    let policy: Arc<MockPolicyEnforcer> = Arc::new(MockPolicyEnforcer::allow_all());
    let tenant_resolver = Arc::new(fakes::FakeTenantResolverClient::with_chain(&[
        caller_tenant,
    ])) as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg = Arc::new(NoopRbacRgRead) as Arc<dyn rbac::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, rg));
    let target_type_validator: Arc<dyn rbac::domain::target_type_validator::TargetTypeValidator> =
        Arc::new(rbac::domain::target_type_validator::AcceptAllTargetTypeValidator::new());
    let service = Arc::new(RoleDefinitionService::new(
        provider,
        Arc::clone(&repo),
        Arc::new(role_assignment_repo::RoleAssignmentRepository),
        policy,
        scope_validator,
        target_type_validator,
    ));
    let state = Arc::new(ApiState { service });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    let router = with_non_root_security_context(router(state, &openapi), caller_tenant);
    Ok((router, repo, fixture))
}

/// No-op RbacRgRead — these tests don't exercise RG scopes.
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

// ---------------------------------------------------------------------------
// Malformed request inputs MUST surface as `application/problem+json`
// (Content-Type) carrying a usable diagnostic, via the `CanonicalJson` /
// `CanonicalPath` wrappers.
//
// Note what this router is: `build_router_with_repo` stops at
// `with_test_security_context`, which adds an `axum::Extension` and nothing
// else — the canonical-error middleware is NOT in this stack. So these cases
// exercise the wrappers directly: without them the rejection arrives as
// `text/plain` and this file's helper panics deserialising it, rather than
// producing the enriched Problem a production router would fall back to. In
// production the middleware does rescue such a body, but into a Problem whose
// `detail` is only the status's reason phrase — which is why the wrappers earn
// their place there too.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn h4_post_with_malformed_json_returns_problem_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;
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
    let body = common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    let violations = body["context"]["field_violations"]
        .as_array()
        .expect("field_violations MUST be present");
    assert_eq!(violations[0]["field"], "body");
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn h4_patch_with_malformed_json_returns_problem_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;
    let response = router
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/rbac/v1/role-definitions/{}", Uuid::now_v7()))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, "irrelevant")
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
async fn h4_get_with_malformed_uuid_returns_problem_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/rbac/v1/role-definitions/not-a-uuid")
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

// ---------------------------------------------------------------------------
// `#[serde(deny_unknown_fields)]` MUST reject misspelled members.
// Without it, `{"permisions": [...]}` deserialises to a no-op patch and
// the response is a misleading 200 OK with "no fields changed".
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn h5_post_with_unknown_field_returns_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;
    let body = serde_json::json!({
        "name": "Auditor",
        "description": "test",
        "permisions": [], // ← typo for "permissions"
        "not_permissions": [],
        "assignable_scopes": [format!("/tenants/{tenant}")],
        "owner_tenant_id": tenant,
    });
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-definitions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).expect("json")))
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
async fn h5_patch_with_unknown_field_returns_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;
    let response = router
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/rbac/v1/role-definitions/{}", Uuid::now_v7()))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, "irrelevant")
                .body(Body::from(r#"{"permisions": []}"#))
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
async fn a1_happy_create_returns_201_with_location_and_etag() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;
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
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a4b_policy_deny_returns_403() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, false).await?;
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

/// A non-root, tenant-bound caller POSTing a role definition that
/// claims a *different* tenant's `owner_tenant_id` MUST be rejected with
/// 403 — `caller_scope_from_context` resolves the caller to
/// `CallerScope::Tenant(caller)` and `resolve_owner_tenant` rejects the
/// cross-tenant body as `OwnerTenantMismatch`. Exercises the
/// `is_first_party_root == false` branch end-to-end through the
/// `with_non_root_security_context` helper. The policy enforcer is
/// `allow_all`, so the
/// rejection is the owner-tenant guard, not an authz denial.
#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn non_root_caller_cross_tenant_create_returns_403() -> Result<()> {
    let caller_tenant = Uuid::now_v7();
    let other_tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_non_root_router(caller_tenant).await?;
    // Body claims `other_tenant`, but the caller is bound to `caller_tenant`.
    let body = serde_json::to_vec(&create_body("Auditor", other_tenant)).expect("json");
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
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a1d_duplicate_name_returns_409() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;
    let body = serde_json::to_vec(&create_body("Auditor", tenant)).expect("json");

    // First insert: 201.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-definitions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.clone()))
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::CREATED);

    // Second insert: 409.
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
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a3_case_insensitive_builtin_collision_returns_409() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;
    let body = create_body("owner", tenant); // collides with built-in 'Owner'
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-definitions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).expect("json")))
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::CONFLICT, "already_exists").await;
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a7d_malformed_filter_returns_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/rbac/v1/role-definitions?%24filter=foo%20eq%20true")
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    Ok(())
}

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a10_get_nonexistent_returns_404() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;
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

// Canonical taxonomy has no 412 / 428: missing If-Match surfaces as
// `FailedPrecondition` (HTTP 400). Callers branch on
// `context.violations[].type = PRECONDITION_REQUIRED`.

#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a12_missing_if_match_returns_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;
    let body = serde_json::to_vec(&create_body("Auditor", tenant)).expect("json");
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
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let id = value["id"].as_str().expect("id field");

    // PATCH without If-Match → 400 (FailedPrecondition).
    let response = router
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/rbac/v1/role-definitions/{id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "description": "new"
                    }))
                    .expect("json"),
                ))
                .expect("build req"),
        )
        .await
        .expect("send");
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

// Single-field validation collapses to `InvalidArgument` (HTTP 400).
#[tokio::test]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a13_immutable_field_in_patch_returns_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;

    let body = serde_json::to_vec(&create_body("Auditor", tenant)).expect("json");
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
    let id = value["id"].as_str().expect("id field");

    let response = router
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/rbac/v1/role-definitions/{id}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, etag)
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "is_built_in": true
                    }))
                    .expect("json"),
                ))
                .expect("build req"),
        )
        .await
        .expect("send");
    let body = common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    let violations = body["context"]["field_violations"]
        .as_array()
        .expect("Problem `context.field_violations` MUST be present for ImmutableFieldRejected");
    assert_eq!(
        violations.len(),
        1,
        "immutable-field rejection MUST carry exactly one field violation"
    );
    assert_eq!(violations[0]["field"], "is_built_in");
    assert_eq!(violations[0]["reason"], "immutable_field_rejected");
    Ok(())
}

// A-14: regression guard for lost-update under concurrent PATCH. Two
// PATCHes carrying the *same* valid v1 `If-Match` race against the same
// row; the SeaORM `UPDATE … WHERE id = ? AND updated_at = ?` CAS in
// `role_definition_repo::update` (`src/infra/storage/repo/role_definition_repo.rs::447-485`)
// MUST let exactly one win (200 OK + advanced ETag) and reject the other.
// The loser is rejected with canonical `FailedPrecondition` = HTTP 400
//  — NOT 412; the stale-precondition nature rides in
// `context.violations[].type == "PRECONDITION_FAILED"` (the
// `OptimisticConcurrencyStale` arm in `src/api/rest/error.rs` calls
// `failed_precondition()`). If the predicate is ever dropped or the flow
// regresses to a read-then-update shape, both PATCHes will succeed and
// the loser's body will silently overwrite the winner — this test fails
// in that case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Phase 4 Postgres integration; run via `cargo test -p cf-gears-rbac -- --ignored`"]
async fn a14_concurrent_patch_same_etag_one_wins_one_400() -> Result<()> {
    let tenant = Uuid::now_v7();
    let (router, _repo, _fixture) = build_router(tenant, true).await?;

    // Seed the row and read the v1 ETag straight off the POST response
    // — no hard-coded value.
    let create_response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rbac/v1/role-definitions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&create_body("Auditor", tenant)).expect("json"),
                ))
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(create_response.status(), StatusCode::CREATED);
    let v1_etag = create_response
        .headers()
        .get(header::ETAG)
        .expect("etag")
        .to_str()
        .expect("utf8")
        .to_owned();
    let bytes = to_bytes(create_response.into_body(), 1_000_000)
        .await
        .expect("body");
    let created: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let id = created["id"].as_str().expect("id field").to_owned();

    // Two distinct patches so the winner is identifiable by `description`.
    let req_a = Request::builder()
        .method("PATCH")
        .uri(format!("/rbac/v1/role-definitions/{id}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::IF_MATCH, v1_etag.as_str())
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "description": "winner-A" })).expect("json"),
        ))
        .expect("build req");
    let req_b = Request::builder()
        .method("PATCH")
        .uri(format!("/rbac/v1/role-definitions/{id}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::IF_MATCH, v1_etag.as_str())
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "description": "winner-B" })).expect("json"),
        ))
        .expect("build req");

    let (resp_a, resp_b) =
        tokio::join!(router.clone().oneshot(req_a), router.clone().oneshot(req_b));
    let resp_a = resp_a.expect("send a");
    let resp_b = resp_b.expect("send b");

    let (sa, sb) = (resp_a.status(), resp_b.status());
    let a_ok = sa == StatusCode::OK;
    let b_ok = sb == StatusCode::OK;
    // The CAS loser is rejected with canonical `FailedPrecondition` = HTTP
    // 400 (NOT 412); the stale-precondition nature is carried by
    // `context.violations[].type`, asserted on the loser body below.
    let a_pre = sa == StatusCode::BAD_REQUEST;
    let b_pre = sb == StatusCode::BAD_REQUEST;
    assert!(
        (a_ok && b_pre) || (b_ok && a_pre),
        "expected exactly one OK and one 400 FailedPrecondition, got a={sa} b={sb}",
    );
    let (winner_resp, loser_resp, expected_description) = if a_ok {
        (resp_a, resp_b, "winner-A")
    } else {
        (resp_b, resp_a, "winner-B")
    };

    // The loser's 400 MUST carry the stale-precondition discriminator so a
    // client can tell it apart from other 400s and retry with a fresh ETag.
    let loser_bytes = to_bytes(loser_resp.into_body(), 1_000_000)
        .await
        .expect("loser body");
    let loser_body: serde_json::Value = serde_json::from_slice(&loser_bytes).expect("json");
    assert_eq!(
        loser_body["context"]["violations"][0]["type"], "PRECONDITION_FAILED",
        "the CAS loser MUST surface a PRECONDITION_FAILED violation"
    );

    let v2_etag = winner_resp
        .headers()
        .get(header::ETAG)
        .expect("etag")
        .to_str()
        .expect("utf8")
        .to_owned();
    assert_ne!(v1_etag, v2_etag, "winner MUST advance the ETag");

    // Re-read the row and confirm it matches the winner — proves the
    // loser's patch did NOT overwrite the winner.
    let get_response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/rbac/v1/role-definitions/{id}"))
                .body(Body::empty())
                .expect("build req"),
        )
        .await
        .expect("send");
    assert_eq!(get_response.status(), StatusCode::OK);
    let get_etag = get_response
        .headers()
        .get(header::ETAG)
        .expect("etag")
        .to_str()
        .expect("utf8")
        .to_owned();
    assert_eq!(
        get_etag, v2_etag,
        "row's current ETag MUST match the winner's response ETag"
    );
    let bytes = to_bytes(get_response.into_body(), 1_000_000)
        .await
        .expect("body");
    let row: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        row["description"], expected_description,
        "stored `description` MUST match the winner's patch"
    );
    Ok(())
}
