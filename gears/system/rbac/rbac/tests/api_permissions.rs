//! API-level tests for `GET /rbac/v1/permissions`.
//!
//! Builds the live Axum router with an in-memory catalog and dispatches
//! single requests via `tower::ServiceExt::oneshot`. No DB, no
//! `ClientHub`, no upstream services.

#![cfg(test)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use common::with_test_security_context;
use rbac::api::rest::permissions::{ApiState, router};
use rbac::domain::permission_catalog::{InMemoryPermissionCatalog, PermissionCatalog};

/// Six RBAC `(action, resource_type)` pairs — the module's catalog
/// inventory. `InMemoryPermissionCatalog::with_pairs` synthesizes an
/// `id` per pair; list results are sorted by that id.
const RBAC_PAIRS: &[(&str, &str)] = &[
    ("read", "gts.cf.core.rbac.role_definition.v1~"),
    ("write", "gts.cf.core.rbac.role_definition.v1~"),
    ("delete", "gts.cf.core.rbac.role_definition.v1~"),
    ("read", "gts.cf.core.rbac.role_assignment.v1~"),
    ("write", "gts.cf.core.rbac.role_assignment.v1~"),
    ("delete", "gts.cf.core.rbac.role_assignment.v1~"),
];

fn build_router_with_pairs(pairs: &[(&str, &str)]) -> Router {
    // Live router rejects unauthenticated calls; wrap in the shared
    // security-context layer for the happy path.
    with_test_security_context(build_unwrapped_router_with_pairs(pairs))
}

/// Without the security-context extension layer — used by auth-guard
/// regression tests that simulate an upstream gateway mis-wire.
fn build_unwrapped_router_with_pairs(pairs: &[(&str, &str)]) -> Router {
    let catalog: Arc<dyn PermissionCatalog> = Arc::new(InMemoryPermissionCatalog::with_pairs(
        pairs
            .iter()
            .map(|(a, r)| ((*a).to_owned(), (*r).to_owned())),
    ));
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    router(Arc::new(ApiState { catalog }), &openapi)
}

async fn parse_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("json parse")
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("build req")
}

/// `OpenAPI` wiring smoke test: `permissions::router(state, &openapi)`
/// MUST cause the registry to capture `GET /rbac/v1/permissions` with
/// the declared `operation_id`. Pins `OperationBuilder` plumbing so a
/// regression cannot silently empty `paths: {}` again.
#[test]
fn openapi_registry_captures_list_permissions_operation() {
    let catalog: Arc<dyn PermissionCatalog> = Arc::new(InMemoryPermissionCatalog::with_pairs(
        std::iter::empty::<(String, String)>(),
    ));
    let openapi = toolkit::api::OpenApiRegistryImpl::new();
    let _router = router(Arc::new(ApiState { catalog }), &openapi);

    let spec = openapi
        .operation_specs
        .get("GET:/rbac/v1/permissions")
        .expect("registry must capture GET /rbac/v1/permissions");
    assert_eq!(
        spec.operation_id.as_deref(),
        Some("rbac.list_permissions"),
        "operation_id MUST match the OperationBuilder declaration"
    );
    assert!(
        spec.tags.iter().any(|t| t == "RBAC Permissions"),
        "operation MUST carry the `RBAC Permissions` tag (got: {:?})",
        spec.tags
    );
    assert!(
        spec.authenticated,
        "list_permissions MUST be marked authenticated"
    );
    // GET has no request body; `request_body` MUST stay `None`.
    assert!(
        spec.request_body.is_none(),
        "list_permissions is a GET with no body; request_body MUST be None"
    );
    // 200 response MUST reference the shared `toolkit_odata::Page`
    // envelope specialised on `AuthzPermissionDto` (schema name
    // `Page_AuthzPermissionDto`).
    let ok_response = spec
        .responses
        .iter()
        .find(|r| r.status == 200)
        .expect("list_permissions MUST declare a 200 response");
    assert!(
        ok_response
            .schema_name()
            .is_some_and(|n| n.contains("AuthzPermissionDto")),
        "200 response MUST reference the Page<AuthzPermissionDto> schema (got {:?})",
        ok_response.schema_name()
    );
    // Canonical `Problem` schema MUST be registered by `error_4xx` /
    // `error_500`.
    let components = openapi.components_registry.load();
    assert!(
        components.contains_key("Problem"),
        "error helpers MUST register the canonical `Problem` schema; got components: {:?}",
        components.keys().collect::<Vec<_>>()
    );
    // The `Page<T>` envelope registers both the item schema and the
    // shared `PageInfo` footer as components.
    assert!(
        components.contains_key("AuthzPermissionDto"),
        "item DTO MUST be registered as an OpenAPI component; got {:?}",
        components.keys().collect::<Vec<_>>()
    );
    assert!(
        components.contains_key("PageInfo"),
        "shared PageInfo footer MUST be registered as an OpenAPI component; got {:?}",
        components.keys().collect::<Vec<_>>()
    );
}

/// A-44: returns 200, six items sorted by `id` ascending, each with all
/// four DTO fields.
#[tokio::test]
async fn a44_list_returns_all_pairs_sorted_by_id() {
    let router = build_router_with_pairs(RBAC_PAIRS);
    let response = router
        .oneshot(get("/rbac/v1/permissions"))
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let ct = response
        .headers()
        .get(header::CONTENT_TYPE)
        .expect("content-type")
        .to_str()
        .expect("utf8")
        .to_owned();
    assert!(
        ct.starts_with("application/json"),
        "expected application/json, got {ct}"
    );

    let body = parse_json(response).await;
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 6, "all six pairs MUST surface");

    // Sorted-by-id-ascending invariant.
    let ids: Vec<&str> = items.iter().map(|i| i["id"].as_str().unwrap()).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "items MUST be sorted by `id` ascending");

    // Every DTO field is present and non-empty.
    for item in items {
        for field in ["id", "resource_type", "action", "display_name"] {
            let v = item
                .get(field)
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("item missing field `{field}`: {item}"));
            assert!(!v.is_empty(), "field `{field}` MUST be non-empty");
        }
    }

    // First page is the entire result set — both cursors null, and the
    // `toolkit_odata::Page` envelope echoes the effective limit.
    assert!(
        body["page_info"]["next_cursor"].is_null(),
        "single full page MUST have a null next_cursor"
    );
    assert!(
        body["page_info"]["prev_cursor"].is_null(),
        "first page MUST have a null prev_cursor"
    );
    assert_eq!(
        body["page_info"]["limit"], 50,
        "page_info MUST echo the default limit (50) when none was supplied"
    );
}

/// A-45: `?action=read` returns only the two `read` pairs.
#[tokio::test]
async fn a45_action_filter_returns_only_matching_pairs() {
    let router = build_router_with_pairs(RBAC_PAIRS);
    let response = router
        .oneshot(get("/rbac/v1/permissions?action=read"))
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body = parse_json(response).await;
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2, "two `read` pairs in the seeded inventory");
    for item in items {
        assert_eq!(item["action"], "read");
    }
}

/// A-46: `resource_type_prefix=gts.cf.core.rbac` matches all six pairs.
#[tokio::test]
async fn a46_resource_type_prefix_filter_matches_subset() {
    let router = build_router_with_pairs(RBAC_PAIRS);
    let response = router
        .oneshot(get(
            "/rbac/v1/permissions?resource_type_prefix=gts.cf.core.rbac",
        ))
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let body = parse_json(response).await;
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 6);
    for item in items {
        let rt = item["resource_type"].as_str().unwrap();
        assert!(
            rt.starts_with("gts.cf.core.rbac"),
            "resource_type `{rt}` MUST match the prefix"
        );
    }
}

/// A-47: cursor pagination over `limit=2` walks every entry without
/// duplication; the final page has `has_more = false`.
#[tokio::test]
async fn a47_cursor_pagination_walks_every_entry_without_overlap() {
    let router = build_router_with_pairs(RBAC_PAIRS);

    // First page.
    let response = router
        .clone()
        .oneshot(get("/rbac/v1/permissions?limit=2"))
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let page1 = parse_json(response).await;
    assert_eq!(page1["items"].as_array().unwrap().len(), 2);
    assert!(
        page1["page_info"]["prev_cursor"].is_null(),
        "first page MUST have a null prev_cursor"
    );
    let cursor1 = page1["page_info"]["next_cursor"]
        .as_str()
        .expect("more items remain \u{2014} next_cursor MUST be present")
        .to_owned();

    // Second page.
    let response = router
        .clone()
        .oneshot(get(&format!(
            "/rbac/v1/permissions?limit=2&cursor={cursor1}"
        )))
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let page2 = parse_json(response).await;
    assert_eq!(page2["items"].as_array().unwrap().len(), 2);
    assert!(
        page2["page_info"]["prev_cursor"].is_string(),
        "middle page MUST carry a prev_cursor"
    );
    let cursor2 = page2["page_info"]["next_cursor"]
        .as_str()
        .expect("more items remain \u{2014} next_cursor MUST be present")
        .to_owned();

    // Third page — final, has_more = false.
    let response = router
        .oneshot(get(&format!(
            "/rbac/v1/permissions?limit=2&cursor={cursor2}"
        )))
        .await
        .expect("send");
    assert_eq!(response.status(), StatusCode::OK);
    let page3 = parse_json(response).await;
    assert_eq!(page3["items"].as_array().unwrap().len(), 2);
    assert!(
        page3["page_info"]["next_cursor"].is_null(),
        "final page MUST have a null next_cursor"
    );
    assert!(
        page3["page_info"]["prev_cursor"].is_string(),
        "final page (reached forward) MUST carry a prev_cursor"
    );

    // No duplicates and no skips across the three pages.
    let mut seen: Vec<&str> = Vec::new();
    for page in [&page1, &page2, &page3] {
        for item in page["items"].as_array().unwrap() {
            seen.push(item["id"].as_str().unwrap());
        }
    }
    assert_eq!(seen.len(), 6, "total items across pages MUST equal 6");
    let mut deduped = seen.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(
        deduped.len(),
        6,
        "no duplicate id may appear across pages: {seen:?}"
    );
}

/// A-47b: `prev_cursor` walks pages in reverse. Page forward to the last
/// page, then follow `prev_cursor` back, asserting each backward page
/// reproduces the corresponding forward page (same ids, same order).
#[tokio::test]
async fn a47b_prev_cursor_walks_pages_in_reverse() {
    // Helper: fetch one page at the given query and return the parsed body.
    async fn fetch(router: &Router, uri: &str) -> serde_json::Value {
        let response = router.clone().oneshot(get(uri)).await.expect("send");
        assert_eq!(response.status(), StatusCode::OK);
        parse_json(response).await
    }

    let router = build_router_with_pairs(RBAC_PAIRS);

    let ids = |page: &serde_json::Value| -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_owned())
            .collect()
    };

    // Forward walk: collect the id sets of all three pages.
    let fwd1 = fetch(&router, "/rbac/v1/permissions?limit=2").await;
    let c1 = fwd1["page_info"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let fwd2 = fetch(
        &router,
        &format!("/rbac/v1/permissions?limit=2&cursor={c1}"),
    )
    .await;
    let c2 = fwd2["page_info"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let fwd3 = fetch(
        &router,
        &format!("/rbac/v1/permissions?limit=2&cursor={c2}"),
    )
    .await;

    // From the last page, walk back via prev_cursor.
    let back_cursor_1 = fwd3["page_info"]["prev_cursor"]
        .as_str()
        .expect("last page MUST carry a prev_cursor")
        .to_owned();
    let back1 = fetch(
        &router,
        &format!("/rbac/v1/permissions?limit=2&cursor={back_cursor_1}"),
    )
    .await;
    assert_eq!(
        ids(&back1),
        ids(&fwd2),
        "backward page from the last page MUST reproduce the middle page"
    );

    // One more step back lands on the first page again.
    let back_cursor_2 = back1["page_info"]["prev_cursor"]
        .as_str()
        .expect("middle page MUST carry a prev_cursor")
        .to_owned();
    let back2 = fetch(
        &router,
        &format!("/rbac/v1/permissions?limit=2&cursor={back_cursor_2}"),
    )
    .await;
    assert_eq!(
        ids(&back2),
        ids(&fwd1),
        "backward page MUST reproduce the first page"
    );
    assert!(
        back2["page_info"]["prev_cursor"].is_null(),
        "first page reached via backward walk MUST have a null prev_cursor"
    );
    assert!(
        back2["page_info"]["next_cursor"].is_string(),
        "backward-reached first page still has entries after it"
    );
}

/// Over-cap `limit` (>200) → 400 `application/problem+json` with the
/// canonical `invalid_argument` type URI and a `field_violations` entry
/// whose `reason = invalid_limit`.
#[tokio::test]
async fn a48_over_cap_limit_returns_400_problem_json() {
    let router = build_router_with_pairs(RBAC_PAIRS);
    let response = router
        .oneshot(get("/rbac/v1/permissions?limit=500"))
        .await
        .expect("send");
    let body = common::assert_problem(response, StatusCode::BAD_REQUEST, "invalid_argument").await;
    let violations = body["context"]["field_violations"]
        .as_array()
        .expect("field_violations array");
    assert_eq!(violations.len(), 1, "field_violations must carry one entry");
    assert_eq!(violations[0]["field"], "limit");
    assert_eq!(violations[0]["reason"], "invalid_limit");
}

// ---------------------------------------------------------------------------
// Auth-guard regression tests — the route MUST reject unauthenticated
// callers even when the upstream gateway is mis-wired.
// ---------------------------------------------------------------------------

/// AUTH-1: missing `Extension<SecurityContext>` MUST surface as 401.
#[tokio::test]
async fn auth_missing_security_context_returns_401() {
    let router = build_unwrapped_router_with_pairs(RBAC_PAIRS);
    let response = router
        .oneshot(get("/rbac/v1/permissions"))
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::UNAUTHORIZED, "unauthenticated").await;
}

/// AUTH-2: anonymous `SecurityContext` MUST surface as 401.
#[tokio::test]
async fn auth_anonymous_security_context_returns_401() {
    use axum::Extension;
    use toolkit_security::SecurityContext;

    let router = build_unwrapped_router_with_pairs(RBAC_PAIRS)
        .layer(Extension(SecurityContext::anonymous()));
    let response = router
        .oneshot(get("/rbac/v1/permissions"))
        .await
        .expect("send");
    common::assert_problem(response, StatusCode::UNAUTHORIZED, "unauthenticated").await;
}

/// AUTH-3: authenticated `SecurityContext` succeeds with 200.
#[tokio::test]
async fn auth_authenticated_security_context_returns_200() {
    let router = build_router_with_pairs(RBAC_PAIRS);
    let response = router
        .oneshot(get("/rbac/v1/permissions"))
        .await
        .expect("send");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "authenticated SecurityContext MUST allow the request"
    );
}
