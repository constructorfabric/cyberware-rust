//! Boundary tests for the AuthN-before-AuthZ invariant — the RBAC
//! module's router rejects requests that arrive without a verified
//! `SecurityContext` (defence in depth against an upstream mis-wire that
//! would otherwise be evaluated as a root-shaped anonymous caller).
//!
//! Two layers of coverage:
//!
//! 1. **Mode coverage** (4 tests on `GET /rbac/v1/role-definitions/{id}`) —
//!    each failure mode of [`require_authenticated`] gets one canonical
//!    test: missing extension, garbage `Authorization` header, anonymous
//!    `SecurityContext`, and `SecurityContext` without `subject_type`.
//! 2. **Endpoint matrix** (9 tests, one per (method, path) pair the
//!    module exposes) — each exercises the "no `SecurityContext`
//!    extension" mode and asserts 401. This catches the regression where
//!    `Extension(ctx): Extension<SecurityContext>` is dropped from a
//!    handler during refactor: without the matrix, removing the auth
//!    check from any handler except the one GET would be silent.

#![cfg(test)]
#![allow(clippy::expect_used, clippy::doc_markdown)]

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;
use uuid::Uuid;

use rbac::api::rest::role_assignments::{
    ApiState as RoleAssignmentsApiState, router as role_assignments_router,
};
use rbac::api::rest::role_definitions::{ApiState, router};
use rbac::domain::policy_enforcer::MockPolicyEnforcer;
use rbac::domain::role_assignment::RoleAssignmentService;
use rbac::domain::role_definition::RoleDefinitionService;
use rbac::domain::scope_validator::ScopeValidator;
use rbac::infra::storage::role_assignment_repo;
use rbac::infra::storage::role_definition_repo;

mod common;
use common::scope_fakes as fakes;

/// Build the deny-all router against a fresh in-memory SQLite provider.
///
/// The AuthN guard decides before any repo call — the file's own module
/// doc states this — so the backend is inert for every assertion here.
/// SQLite rather than a PostgreSQL testcontainer, so the 401 contract for
/// every route is verified in a default `cargo test` run instead of behind
/// `#[ignore]`.
async fn build_deny_all_router() -> Result<Router> {
    let provider = common::fresh_sqlite_provider().await?;
    let repo = Arc::new(role_definition_repo::RoleDefinitionRepository);
    let policy: Arc<MockPolicyEnforcer> = Arc::new(MockPolicyEnforcer::deny_all());
    let tenant = Uuid::new_v4();
    let tenant_resolver = Arc::new(fakes::FakeTenantResolverClient::with_chain(&[tenant]))
        as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg = Arc::new(NoopRbacRgRead) as Arc<dyn rbac::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, rg));
    let target_type_validator: Arc<dyn rbac::domain::target_type_validator::TargetTypeValidator> =
        Arc::new(rbac::domain::target_type_validator::AcceptAllTargetTypeValidator::new());
    let service = Arc::new(RoleDefinitionService::new(
        provider,
        Arc::clone(&repo),
        // The 401 boundary is decided before any repo read; the count store
        // is present only to satisfy the constructor. It is the real
        // repository because `ApiState` names the concrete types — the
        // repo traits take `<C: DBRunner>` and cannot be trait objects.
        Arc::new(role_assignment_repo::RoleAssignmentRepository),
        policy,
        scope_validator,
        Arc::clone(&target_type_validator),
    ));
    let state = Arc::new(ApiState { service });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    Ok(router(state, &openapi))
}

/// Sibling of [`build_deny_all_router`] for the role-assignments router.
/// Same deny-all posture; the auth guard fires before any repo / RG /
/// policy call, so the backing services are inert.
async fn build_deny_all_role_assignments_router() -> Result<Router> {
    // Two independent providers mirror the production wiring used by
    // `api_role_assignments.rs` — assignments and definitions own their
    // own connection handles. Each `fresh_sqlite_provider` call opens its
    // own in-memory database, which is fine here: the auth guard fires
    // before either is touched.
    let provider_assignments = common::fresh_sqlite_provider().await?;
    let _provider_definitions = common::fresh_sqlite_provider().await?;
    let assignment_repo = Arc::new(role_assignment_repo::RoleAssignmentRepository);
    let role_repo = Arc::new(role_definition_repo::RoleDefinitionRepository);
    let policy: Arc<MockPolicyEnforcer> = Arc::new(MockPolicyEnforcer::deny_all());
    let tenant = Uuid::new_v4();
    let tenant_resolver = Arc::new(fakes::FakeTenantResolverClient::with_chain(&[tenant]))
        as Arc<dyn tenant_resolver_sdk::TenantResolverClient>;
    let rg = Arc::new(NoopRbacRgRead) as Arc<dyn rbac::domain::rg_port::RbacRgRead>;
    let scope_validator = Arc::new(ScopeValidator::new(tenant_resolver, Arc::clone(&rg)));
    let service = Arc::new(RoleAssignmentService::new(
        provider_assignments.clone(),
        Arc::clone(&assignment_repo),
        Arc::clone(&role_repo),
        policy,
        scope_validator,
        rg,
    ));
    let state = Arc::new(RoleAssignmentsApiState { service });
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    Ok(role_assignments_router(state, &openapi))
}

/// Which resource router a matrix entry targets.
#[derive(Clone, Copy)]
enum Resource {
    RoleDef,
    RoleAsg,
}

/// Drive a single (method, path) cell of the auth-boundary matrix: build
/// the matching deny-all router with **no** `SecurityContext` extension
/// layer, send the request, assert 401.
///
/// `body` is `Some(json)` for POST/PATCH so the `Json` extractor parses
/// successfully and the handler body runs; otherwise the 400/415 from a
/// failed body extraction would mask a missing `require_authenticated`
/// call. `Content-Type: application/json` is set automatically when a
/// body is provided.
async fn assert_unauthenticated_yields_401(
    method: &'static str,
    path: &'static str,
    resource: Resource,
    body: Option<&'static str>,
) -> Result<()> {
    let app = match resource {
        Resource::RoleDef => build_deny_all_router().await?,
        Resource::RoleAsg => build_deny_all_role_assignments_router().await?,
    };
    let mut builder = Request::builder().uri(path).method(method);
    let req_body = match body {
        Some(json) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(json)
        }
        None => Body::empty(),
    };
    let req = builder.body(req_body).expect("build request");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "{method} {path}: missing SecurityContext must produce 401 (got {})",
        resp.status()
    );
    Ok(())
}

// Minimal request bodies whose `Json` extraction succeeds — auth fails
// in the handler body, before the service call. Values are dummies; the
// service is never reached.
const POST_ROLE_DEFINITION_BODY: &str = r#"{"name":"x","assignable_scopes":[]}"#;
const PATCH_ROLE_DEFINITION_BODY: &str = r"{}";
const POST_ROLE_ASSIGNMENT_BODY: &str = concat!(
    r#"{"role_definition_id":"00000000-0000-0000-0000-000000000000","#,
    r#""principal_id":"x","principal_type":"User","scope":"/"}"#,
);

/// No-op RbacRgRead — boundary tests don't exercise RG scopes.
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

#[tokio::test]
async fn router_returns_401_without_security_context() -> Result<()> {
    // No SecurityContext extension → 401 (would otherwise fall through
    // to root-shaped anonymous and be a defence-in-depth gap).
    let app = build_deny_all_router().await?;
    let req = Request::builder()
        .uri("/rbac/v1/role-definitions/00000000-0000-0000-0000-000000000000")
        .method("GET")
        .body(Body::empty())
        .expect("build request");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "missing SecurityContext must produce 401"
    );
    Ok(())
}

#[tokio::test]
async fn router_returns_401_with_garbage_authorization_header() -> Result<()> {
    // The module doesn't parse `Authorization`; with no SecurityContext
    // extension the request is 401 regardless of header content.
    let app = build_deny_all_router().await?;
    let req = Request::builder()
        .uri("/rbac/v1/role-definitions/00000000-0000-0000-0000-000000000000")
        .method("GET")
        .header(header::AUTHORIZATION, "Bearer not-a-jwt")
        .body(Body::empty())
        .expect("build request");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "missing SecurityContext must produce 401 even with an Authorization header"
    );
    Ok(())
}

#[tokio::test]
async fn router_returns_401_with_anonymous_security_context() -> Result<()> {
    // Explicit anonymous SecurityContext MUST be rejected — same posture
    // as the missing-extension case.
    let router = build_deny_all_router().await?;
    let app = router.layer(axum::Extension(
        toolkit_security::SecurityContext::anonymous(),
    ));
    let req = Request::builder()
        .uri("/rbac/v1/role-definitions/00000000-0000-0000-0000-000000000000")
        .method("GET")
        .body(Body::empty())
        .expect("build request");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "anonymous SecurityContext must produce 401"
    );
    Ok(())
}

#[tokio::test]
async fn router_returns_401_when_subject_type_is_missing() -> Result<()> {
    // A context with non-nil ids but no `subject_type` claim MUST still
    // be 401 — a buggy upstream that fakes ids would also need to
    // fabricate `subject_type` to slip past this guard.
    let ctx_no_subject_type = toolkit_security::SecurityContext::builder()
        .subject_id(uuid::uuid!("33333333-4444-5555-6666-777777777777"))
        .subject_tenant_id(uuid::uuid!("88888888-9999-aaaa-bbbb-cccccccccccc"))
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("build SecurityContext without subject_type");

    let router = build_deny_all_router().await?;
    let app = router.layer(axum::Extension(ctx_no_subject_type));
    let req = Request::builder()
        .uri("/rbac/v1/role-definitions/00000000-0000-0000-0000-000000000000")
        .method("GET")
        .body(Body::empty())
        .expect("build request");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "SecurityContext without subject_type must produce 401"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Endpoint matrix — one test per (method, path) the module exposes.
//
// Each test exercises the canonical "no `SecurityContext` extension"
// failure mode against a distinct handler. If `require_authenticated` is
// dropped from any handler during refactor, the matching matrix test
// flips from 401 to whatever the de-authed code path returns (403, 404,
// 500, …), catching the regression that the mode-coverage tests above
// would miss because they only cover one GET.
//
// Body shape for POST/PATCH is the minimal JSON that deserializes into
// the request DTO — see the `*_BODY` consts above. Without a parseable
// body the `Json` extractor would 400 the request *before* the handler
// body runs, silently masking a missing auth check.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn matrix_post_role_definitions_is_401() -> Result<()> {
    assert_unauthenticated_yields_401(
        "POST",
        "/rbac/v1/role-definitions",
        Resource::RoleDef,
        Some(POST_ROLE_DEFINITION_BODY),
    )
    .await
}

#[tokio::test]
async fn matrix_get_role_definitions_list_is_401() -> Result<()> {
    assert_unauthenticated_yields_401("GET", "/rbac/v1/role-definitions", Resource::RoleDef, None)
        .await
}

#[tokio::test]
async fn matrix_get_role_definitions_item_is_401() -> Result<()> {
    assert_unauthenticated_yields_401(
        "GET",
        "/rbac/v1/role-definitions/00000000-0000-0000-0000-000000000000",
        Resource::RoleDef,
        None,
    )
    .await
}

#[tokio::test]
async fn matrix_patch_role_definitions_item_is_401() -> Result<()> {
    assert_unauthenticated_yields_401(
        "PATCH",
        "/rbac/v1/role-definitions/00000000-0000-0000-0000-000000000000",
        Resource::RoleDef,
        Some(PATCH_ROLE_DEFINITION_BODY),
    )
    .await
}

#[tokio::test]
async fn matrix_delete_role_definitions_item_is_401() -> Result<()> {
    assert_unauthenticated_yields_401(
        "DELETE",
        "/rbac/v1/role-definitions/00000000-0000-0000-0000-000000000000",
        Resource::RoleDef,
        None,
    )
    .await
}

#[tokio::test]
async fn matrix_post_role_assignments_is_401() -> Result<()> {
    assert_unauthenticated_yields_401(
        "POST",
        "/rbac/v1/role-assignments",
        Resource::RoleAsg,
        Some(POST_ROLE_ASSIGNMENT_BODY),
    )
    .await
}

#[tokio::test]
async fn matrix_get_role_assignments_list_is_401() -> Result<()> {
    assert_unauthenticated_yields_401("GET", "/rbac/v1/role-assignments", Resource::RoleAsg, None)
        .await
}

#[tokio::test]
async fn matrix_get_role_assignments_item_is_401() -> Result<()> {
    assert_unauthenticated_yields_401(
        "GET",
        "/rbac/v1/role-assignments/00000000-0000-0000-0000-000000000000",
        Resource::RoleAsg,
        None,
    )
    .await
}

#[tokio::test]
async fn matrix_delete_role_assignments_item_is_401() -> Result<()> {
    assert_unauthenticated_yields_401(
        "DELETE",
        "/rbac/v1/role-assignments/00000000-0000-0000-0000-000000000000",
        Resource::RoleAsg,
        None,
    )
    .await
}
