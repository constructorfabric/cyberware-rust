#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use github_mirror::api::rest::routes::register_routes;
use toolkit::api::OpenApiRegistryImpl;
use tower::ServiceExt;

async fn test_router(api_base_url: &str) -> Router {
    let service = common::service(api_base_url).await;
    let openapi = OpenApiRegistryImpl::new();
    register_routes(Router::new(), &openapi, service)
}

async fn body_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn health_returns_200_with_gear_identity() {
    let router = test_router("https://api.github.com").await;

    let request = Request::builder()
        .uri("/github-mirror/v1/health")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["gear"], "github-mirror");
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn the_anonymous_health_body_never_carries_the_upstream_host() {
    let router = test_router("https://github.example.corp/api/v3").await;

    let request = Request::builder()
        .uri("/github-mirror/v1/health")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;

    // `/health` is anonymous, so the configured upstream is deployment
    // detail an unauthenticated caller must not learn - not under its own
    // field name, and not anywhere else in the body either.
    assert!(json.get("api_base_url").is_none(), "{json:?}");
    let rendered = json.to_string();
    assert!(
        !rendered.contains("github.example.corp"),
        "the configured host leaked into the anonymous body: {rendered}"
    );

    let keys: Vec<&String> = json
        .as_object()
        .expect("an object")
        .keys()
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        vec!["gear", "version"],
        "the anonymous contract is the gear's identity and nothing more"
    );
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let router = test_router("https://api.github.com").await;

    let request = Request::builder()
        .uri("/github-mirror/v1/nope")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
