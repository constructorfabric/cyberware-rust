#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_gts::gts_id;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use crate::domain::ports::metrics::CredStoreMetricsPort;
use crate::domain::ports::plugin::PluginSelector;
use crate::domain::resolver::TenantDirectory;
use crate::domain::secret::repo::SecretRepo;
use crate::domain::secret::service::{GcSettings, Service};
use crate::domain::secret::test_support::{
    FakeDir, FakeMetrics, FakePlugin, FakePluginSelector, FakeSecretRepo, catalog_type_resolver,
    make_ctx, mock_enforcer,
};
use credstore_sdk::{SecretRef, SecretType, SecretValue, SharingMode};

use super::register_routes;

// ── Harness helpers ──────────────────────────────────────────────────────────

fn test_subject() -> Uuid {
    Uuid::from_u128(0xAAAA)
}

fn test_tenant() -> Uuid {
    Uuid::from_u128(0xBBBB)
}

fn test_ctx() -> SecurityContext {
    make_ctx(test_subject(), test_tenant())
}

struct TestHarness {
    router: Router,
    repo: Arc<FakeSecretRepo>,
    svc: Arc<Service>,
}

fn build_harness() -> TestHarness {
    let repo = Arc::new(FakeSecretRepo::new());
    let plugin = FakePlugin::new();
    let selector = Arc::new(FakePluginSelector::new(plugin));
    let enforcer = mock_enforcer();
    let dir = Arc::new(FakeDir::single(test_tenant()));
    let metrics = FakeMetrics::new();
    let svc = Arc::new(Service::new(
        Arc::clone(&repo) as Arc<dyn SecretRepo>,
        dir as Arc<dyn TenantDirectory>,
        enforcer,
        selector as Arc<dyn PluginSelector>,
        catalog_type_resolver(),
        metrics as Arc<dyn CredStoreMetricsPort>,
        GcSettings {
            pending_max_age_secs: 3600,
            batch_size: 256,
        },
    ));
    let openapi = OpenApiRegistryImpl::new();
    let router = register_routes(Router::new(), &openapi, Arc::clone(&svc));
    TestHarness { router, repo, svc }
}

/// Build a JSON request with the `SecurityContext` injected as an extension.
fn json_request(
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    ctx: SecurityContext,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let body_bytes = match body {
        Some(json) => Body::from(serde_json::to_vec(&json).unwrap()),
        None => Body::empty(),
    };
    let mut req = builder.body(body_bytes).unwrap();
    req.extensions_mut().insert(ctx);
    req
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

// ── Seed helpers ─────────────────────────────────────────────────────────────

/// Seed an active `Tenant`-shared row through the real write protocol
/// (`Service::put`) — the value's fingerprint must match the fence key the
/// service lazily bootstraps, so seeding through the same path the router
/// uses (rather than fabricating a `SecretRow`/plugin entry by hand) is what
/// keeps `GET` able to verify it. Returns the row id — the generation half of
/// the `"<id>.<version>"` validator the `If-Match` tests build.
async fn seed_secret(harness: &TestHarness, reference: &str, value: &str) -> Uuid {
    use crate::domain::secret::model::WriteSpec;
    let key = SecretRef::new(reference).expect("valid ref");
    harness
        .svc
        .put(
            &test_ctx(),
            &key,
            SecretValue::from(value),
            WriteSpec::create(SharingMode::Tenant),
        )
        .await
        .expect("seed via the real write protocol");
    harness
        .repo
        .rows()
        .into_iter()
        .find(|r| r.reference == reference)
        .expect("seeded row present")
        .id
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn post_create_returns_201_with_location() {
    let h = build_harness();
    let req = json_request(
        "POST",
        "/credstore/v1/secrets",
        Some(serde_json::json!({
            "reference": "mykey",
            "value": "mysecret",
            "sharing": "tenant"
        })),
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let location = resp
        .headers()
        .get(axum::http::header::LOCATION)
        .expect("Location header")
        .to_str()
        .expect("ascii")
        .to_owned();
    assert!(
        location.ends_with("/mykey"),
        "Location must end with /mykey, got {location}"
    );
}

#[tokio::test]
async fn post_type_is_full_gts_id_only() {
    // The `type` field is a full GTS type id only (PR #4204 review C9): the
    // catalog short name and the type's raw UUID are no longer accepted, and
    // the response echoes the full GTS id.
    let api_key = SecretType::from_name("api-key").expect("known");

    let h = build_harness();
    let req = json_request(
        "POST",
        "/credstore/v1/secrets",
        Some(serde_json::json!({
            "reference": "byid",
            "value": "v",
            "sharing": "tenant",
            "type": api_key.gts_id()
        })),
        test_ctx(),
    );
    let resp = h.router.clone().oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::CREATED);

    let get = json_request("GET", "/credstore/v1/secrets/byid", None, test_ctx());
    let resp = h.router.oneshot(get).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["metadata"]["type"], api_key.gts_id());

    // Short name and raw UUID are not GTS type ids → rejected at the transport.
    for bad in ["api-key".to_owned(), api_key.uuid().to_string()] {
        let h = build_harness();
        let req = json_request(
            "POST",
            "/credstore/v1/secrets",
            Some(serde_json::json!({
                "reference": "badtype",
                "value": "v",
                "sharing": "tenant",
                "type": bad
            })),
            test_ctx(),
        );
        let resp = h.router.clone().oneshot(req).await.expect("router");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[tokio::test]
async fn post_unknown_custom_type_returns_400_unknown_secret_type() {
    // A well-formed but unregistered custom GTS type id passes transport
    // parsing and is rejected by the (catalog-backed test) resolver with
    // the same 400 the registry-driven resolver produces.
    let h = build_harness();
    let req = json_request(
        "POST",
        "/credstore/v1/secrets",
        Some(serde_json::json!({
            "reference": "customkey",
            "value": "v",
            "sharing": "tenant",
            "type": gts_id!("cf.core.credstore.secret.v1~acme.connectors.creds.db_password.v1~")
        })),
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(
        body["context"]["field_violations"][0]["reason"], "UNKNOWN_SECRET_TYPE",
        "body: {body}"
    );
}

#[tokio::test]
async fn post_duplicate_returns_409() {
    let h = build_harness();
    // First create
    let req1 = json_request(
        "POST",
        "/credstore/v1/secrets",
        Some(serde_json::json!({
            "reference": "dupkey",
            "value": "v1",
            "sharing": "tenant"
        })),
        test_ctx(),
    );
    let r1 = h.router.clone().oneshot(req1).await.expect("router");
    assert_eq!(r1.status(), StatusCode::CREATED);

    // Second create — should conflict
    let req2 = json_request(
        "POST",
        "/credstore/v1/secrets",
        Some(serde_json::json!({
            "reference": "dupkey",
            "value": "v2",
            "sharing": "tenant"
        })),
        test_ctx(),
    );
    let r2 = h.router.oneshot(req2).await.expect("router");
    assert_eq!(r2.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn get_existing_returns_200_with_body() {
    let h = build_harness();
    seed_secret(&h, "getkey", "hello-world").await;

    let req = json_request("GET", "/credstore/v1/secrets/getkey", None, test_ctx());
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["value"], "hello-world");
    assert_eq!(body["metadata"]["sharing"], "tenant");
    assert_eq!(body["metadata"]["is_inherited"], false);
}

#[tokio::test]
async fn get_response_is_not_cacheable() {
    let h = build_harness();
    seed_secret(&h, "cachekey", "topsecret").await;

    let req = json_request("GET", "/credstore/v1/secrets/cachekey", None, test_ctx());
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let cc = resp
        .headers()
        .get(axum::http::header::CACHE_CONTROL)
        .expect("GET secret must set Cache-Control")
        .to_str()
        .expect("ascii");
    assert!(
        cc.contains("no-store"),
        "secret material must not be cached; Cache-Control was {cc:?}"
    );
}

#[tokio::test]
async fn get_missing_returns_404() {
    let h = build_harness();
    let req = json_request("GET", "/credstore/v1/secrets/nokey", None, test_ctx());
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Build a request with an `If-Match` header set.
fn json_request_if_match(
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    if_match: &str,
    ctx: SecurityContext,
) -> Request<Body> {
    let mut req = json_request(method, uri, body, ctx);
    req.headers_mut().insert(
        axum::http::header::IF_MATCH,
        axum::http::HeaderValue::from_str(if_match).expect("ascii"),
    );
    req
}

#[tokio::test]
async fn put_with_matching_if_match_returns_204() {
    let h = build_harness();
    let row_id = seed_secret(&h, "ocp", "old").await; // seeded at version 1

    let req = json_request_if_match(
        "PUT",
        "/credstore/v1/secrets/ocp",
        Some(serde_json::json!({ "value": "new", "sharing": "tenant" })),
        &format!("\"{row_id}.1\""),
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn put_with_stale_if_match_returns_409() {
    let h = build_harness();
    let row_id = seed_secret(&h, "ocp", "old").await; // version 1

    let req = json_request_if_match(
        "PUT",
        "/credstore/v1/secrets/ocp",
        Some(serde_json::json!({ "value": "new", "sharing": "tenant" })),
        &format!("\"{row_id}.999\""),
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn put_with_malformed_if_match_returns_400() {
    let h = build_harness();
    seed_secret(&h, "ocp", "old").await;

    let req = json_request_if_match(
        "PUT",
        "/credstore/v1/secrets/ocp",
        Some(serde_json::json!({ "value": "new", "sharing": "tenant" })),
        "not-a-valid-etag",
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_with_multivalued_if_match_matches_any_validator() {
    // RFC 7232 §3.1: a multi-valued If-Match matches if ANY listed validator
    // matches. A stale one alongside the current one must still commit.
    let h = build_harness();
    let row_id = seed_secret(&h, "ocp", "old").await; // version 1

    let req = json_request_if_match(
        "PUT",
        "/credstore/v1/secrets/ocp",
        Some(serde_json::json!({ "value": "new", "sharing": "tenant" })),
        &format!("\"{}.1\", \"{row_id}.1\"", uuid::Uuid::new_v4()),
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn put_with_multivalued_if_match_none_matching_returns_409() {
    // A list where no validator matches the current row is a failed
    // precondition, not a spurious 400.
    let h = build_harness();
    let row_id = seed_secret(&h, "ocp", "old").await; // version 1

    let req = json_request_if_match(
        "PUT",
        "/credstore/v1/secrets/ocp",
        Some(serde_json::json!({ "value": "new", "sharing": "tenant" })),
        &format!("\"{row_id}.99\", \"{}.1\"", uuid::Uuid::new_v4()),
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn delete_with_stale_if_match_returns_409() {
    let h = build_harness();
    let row_id = seed_secret(&h, "ocd", "bye").await; // version 1

    let req = json_request_if_match(
        "DELETE",
        "/credstore/v1/secrets/ocd",
        None,
        &format!("\"{row_id}.999\""),
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn put_existing_with_if_match_star_returns_204() {
    let h = build_harness();
    seed_secret(&h, "putkey", "old-value").await;

    // `If-Match: *` — the explicit last-writer-wins overwrite.
    let req = json_request_if_match(
        "PUT",
        "/credstore/v1/secrets/putkey",
        Some(serde_json::json!({
            "value": "new-value",
            "sharing": "tenant"
        })),
        "*",
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn put_without_if_match_returns_400_if_match_required() {
    // `If-Match` is mandatory: an update must state its concurrency stance.
    // The reason code distinguishes "absent" from "malformed" (INVALID_IF_MATCH).
    let h = build_harness();
    seed_secret(&h, "putkey", "old-value").await;

    let req = json_request(
        "PUT",
        "/credstore/v1/secrets/putkey",
        Some(serde_json::json!({ "value": "new-value", "sharing": "tenant" })),
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(
        body["context"]["field_violations"][0]["reason"], "IF_MATCH_REQUIRED",
        "body: {body}"
    );
}

#[tokio::test]
async fn put_missing_target_returns_409_never_creates() {
    // A PUT never creates: the mandatory precondition requires an existing
    // target, so even `If-Match: *` on a missing reference is a 409. Create
    // goes through POST.
    let h = build_harness();
    let req = json_request_if_match(
        "PUT",
        "/credstore/v1/secrets/absent",
        Some(serde_json::json!({ "value": "v", "sharing": "tenant" })),
        "*",
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn delete_existing_with_if_match_star_returns_204() {
    let h = build_harness();
    seed_secret(&h, "delkey", "bye").await;

    let req = json_request_if_match(
        "DELETE",
        "/credstore/v1/secrets/delkey",
        None,
        "*",
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_without_if_match_returns_400_if_match_required() {
    let h = build_harness();
    seed_secret(&h, "delkey", "bye").await;

    let req = json_request("DELETE", "/credstore/v1/secrets/delkey", None, test_ctx());
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(
        body["context"]["field_violations"][0]["reason"], "IF_MATCH_REQUIRED",
        "body: {body}"
    );
}

#[tokio::test]
async fn delete_missing_returns_404() {
    let h = build_harness();
    let req = json_request_if_match(
        "DELETE",
        "/credstore/v1/secrets/ghost",
        None,
        "*",
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn invalid_ref_returns_400() {
    let h = build_harness();
    // "has:colon" contains a colon which is invalid per SecretRef::new
    let req = json_request("GET", "/credstore/v1/secrets/has%3Acolon", None, test_ctx());
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn invalid_ref_on_delete_returns_400() {
    let h = build_harness();
    // `If-Match` present so the 400 exercises reference validation, not the
    // mandatory-precondition gate.
    let req = json_request_if_match(
        "DELETE",
        "/credstore/v1/secrets/has%3Acolon",
        None,
        "*",
        test_ctx(),
    );
    let resp = h.router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
