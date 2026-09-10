#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use github_mirror::api::rest::routes::{ConcreteService, register_routes};
use github_mirror::domain::repo::{LabelRecord, RepoRecord};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

fn repo_record() -> RepoRecord {
    RepoRecord {
        node_id: None,
        id: 970,
        owner: "acme".to_owned(),
        name: "widget".to_owned(),
        full_name: "acme/widget".to_owned(),
        default_branch: "main".to_owned(),
        private: false,
        pushed_at: None,
        stars: 0,
        forks: 0,
        description: None,
        clone_url: None,
    }
}

fn label_record(id: i64, name: &str, color: &str) -> LabelRecord {
    LabelRecord {
        id,
        repo_id: 0,
        name: name.to_owned(),
        color: color.to_owned(),
        is_default: false,
        description: None,
    }
}

fn router_for(service: Arc<ConcreteService>, ctx: SecurityContext) -> Router {
    let openapi = OpenApiRegistryImpl::new();
    register_routes(Router::new(), &openapi, service).layer(axum::Extension(ctx))
}

async fn body_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn get(router: Router, uri: &str) -> axum::http::Response<Body> {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    router.oneshot(request).await.unwrap()
}

#[tokio::test]
async fn labels_sharing_a_name_page_without_repeats_or_gaps() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    // `gm_labels` has only a non-unique `(tenant_id, repo_id)` index, so two
    // labels in one repository may share a name. Ordering by name alone
    // leaves their relative order to the database, and a row can then repeat
    // or vanish between adjacent page windows.
    for id in [301, 302] {
        service
            .upsert_label(&ctx, "acme", "widget", label_record(id, "twin", "ff0000"))
            .await
            .expect("label seed must succeed");
    }

    let router = router_for(service, ctx);

    let mut seen = Vec::new();
    for page in 1..=2 {
        let json = body_json(
            get(
                router.clone(),
                &format!("/repos/acme/widget/labels?per_page=1&page={page}"),
            )
            .await,
        )
        .await;
        let items = json.as_array().expect("items");
        assert_eq!(items.len(), 1, "one row per page: {json:?}");
        seen.push(items[0]["id"].as_i64().expect("an id"));
    }

    seen.sort_unstable();
    assert_eq!(
        seen,
        vec![301, 302],
        "two windows over two equally named labels must return each exactly once"
    );
}

#[tokio::test]
async fn labels_are_listed_by_name() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_label(&ctx, "acme", "widget", label_record(2, "wontfix", "ffffff"))
        .await
        .expect("label seed must succeed");
    service
        .upsert_label(&ctx, "acme", "widget", label_record(1, "bug", "d73a4a"))
        .await
        .expect("label seed must succeed");

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/widget/labels").await;

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let items = json.as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["name"], "bug");
    assert_eq!(items[1]["name"], "wontfix");
    assert_eq!(items[0]["color"], "d73a4a");
}

#[tokio::test]
async fn labels_of_unknown_repository_return_404() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/nope/labels").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn labels_are_tenant_scoped() {
    let owner = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;
    let service = common::service_over(db.clone(), "https://api.github.com");
    service
        .upsert_repo(&owner, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_label(
            &owner,
            "acme",
            "widget",
            label_record(3, "secret", "000000"),
        )
        .await
        .expect("label seed must succeed");

    let stranger = common::caller_in(Uuid::new_v4());
    let router = router_for(service, stranger);
    let response = get(router, "/repos/acme/widget/labels").await;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "another tenant must not even learn the repository is mirrored"
    );
}

#[tokio::test]
async fn label_upsert_is_idempotent_by_id() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_label(&ctx, "acme", "widget", label_record(4, "bug", "d73a4a"))
        .await
        .expect("first upsert must succeed");

    let mut updated = label_record(4, "bug", "b60205");
    updated.description = Some("triaged".to_owned());
    service
        .upsert_label(&ctx, "acme", "widget", updated)
        .await
        .expect("second upsert must succeed");

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/widget/labels").await;
    let json = body_json(response).await;
    let items = json.as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["color"], "b60205");
    assert_eq!(items[0]["description"], "triaged");
}
