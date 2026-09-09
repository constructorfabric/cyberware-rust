#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use github_mirror::api::rest::routes::{ConcreteService, register_routes};
use github_mirror::domain::repo::RepoRecord;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_db::Db;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

fn record(id: i64, name: &str) -> RepoRecord {
    RepoRecord {
        node_id: None,
        id,
        owner: "constructorfabric".to_owned(),
        name: name.to_owned(),
        full_name: format!("constructorfabric/{name}"),
        default_branch: "main".to_owned(),
        private: false,
        pushed_at: Some("2026-08-18T00:00:00Z".to_owned()),
        stars: 3,
        forks: 1,
        description: Some("mirrored".to_owned()),
        clone_url: None,
    }
}

/// Router with the caller's `SecurityContext` injected the way api-gateway does.
fn router_for(service: Arc<ConcreteService>, ctx: SecurityContext) -> Router {
    let openapi = OpenApiRegistryImpl::new();
    register_routes(Router::new(), &openapi, service).layer(axum::Extension(ctx))
}

async fn seeded(db: Db, ctx: &SecurityContext) -> Arc<ConcreteService> {
    let service = common::service_over(db, "https://api.github.com");
    service
        .upsert_repo(ctx, record(101, "gears-rust"))
        .await
        .expect("seed must succeed");
    service
        .upsert_repo(ctx, record(102, "github-repotap"))
        .await
        .expect("seed must succeed");
    service
}

async fn body_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn list_repos_returns_rows_from_the_mirrored_store() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = seeded(common::inmem_db().await, &ctx).await;
    let router = router_for(service, ctx);

    let request = Request::builder()
        .uri("/github-mirror/v1/repos")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let items = json["items"].as_array().expect("payload must carry items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["full_name"], "constructorfabric/gears-rust");
    assert_eq!(items[0]["id"], 101);
    assert_eq!(items[1]["full_name"], "constructorfabric/github-repotap");
}

#[tokio::test]
async fn list_repos_is_tenant_scoped() {
    let owner = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;
    let service = seeded(db.clone(), &owner).await;

    let stranger = common::caller_in(Uuid::new_v4());
    let router = router_for(service, stranger);

    let request = Request::builder()
        .uri("/github-mirror/v1/repos")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert!(
        json["items"].as_array().expect("items").is_empty(),
        "another tenant must not see mirrored rows"
    );
}

#[tokio::test]
async fn list_repos_honours_the_limit_query_param() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = seeded(common::inmem_db().await, &ctx).await;
    let router = router_for(service, ctx);

    let request = Request::builder()
        .uri("/github-mirror/v1/repos?limit=1")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["items"].as_array().expect("items").len(), 1);
}

#[tokio::test]
async fn an_oversized_filter_is_refused_before_it_reaches_sql() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = seeded(common::inmem_db().await, &ctx).await;
    let router = router_for(service, ctx);

    // One node per value: an `in` list this long is past the platform's
    // 2,000-node complexity budget.
    let values = (0..3_000)
        .map(|n| format!("'r{n}'"))
        .collect::<Vec<_>>()
        .join(",");
    let request = Request::builder()
        .uri(format!(
            "/github-mirror/v1/repos?$filter=name%20in%20({values})"
        ))
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "an oversized filter must be refused, not translated to SQL"
    );

    // Same for a filter that is merely enormous rather than deeply nested.
    let long = "a".repeat(9 * 1024);
    let request = Request::builder()
        .uri(format!(
            "/github-mirror/v1/repos?$filter=name%20eq%20'{long}'"
        ))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "filter too long"
    );
}

#[tokio::test]
async fn an_oversized_page_request_is_clamped_to_the_configured_maximum() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = seeded(common::inmem_db().await, &ctx).await;
    let router = router_for(service, ctx);

    let request = Request::builder()
        .uri("/github-mirror/v1/repos?limit=1000000")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert!(
        json["items"].as_array().expect("items").len() <= 200,
        "the store holds fewer rows than the 200-row cap, so a huge limit          must come back as the seeded rows and never as an unbounded read: {json:?}"
    );
}

#[tokio::test]
async fn list_repos_returns_empty_page_on_a_clean_store() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    let router = router_for(service, ctx);

    let request = Request::builder()
        .uri("/github-mirror/v1/repos")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert!(json["items"].as_array().expect("items").is_empty());
}
