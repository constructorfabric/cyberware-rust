#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use github_mirror::api::rest::routes::{ConcreteService, register_routes};
use github_mirror::domain::repo::{MilestoneRecord, RepoRecord};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

fn repo_record() -> RepoRecord {
    RepoRecord {
        node_id: None,
        id: 980,
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

fn milestone_record(id: i64, number: i64, title: &str) -> MilestoneRecord {
    MilestoneRecord {
        id,
        repo_id: 0,
        number,
        title: title.to_owned(),
        state: "open".to_owned(),
        description: None,
        open_issues: 1,
        closed_issues: 0,
        due_on: None,
        created_at: "2026-08-20T00:00:00Z".to_owned(),
        updated_at: "2026-08-20T00:00:00Z".to_owned(),
        closed_at: None,
        html_url: None,
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
async fn milestones_sharing_a_number_page_without_repeats_or_gaps() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    // `gm_milestones` has only a non-unique `(tenant_id, repo_id)` index, so
    // two milestones in one repository may share a number. Ordering by
    // number alone leaves their relative order to the database, and a row can
    // then repeat or vanish between adjacent page windows.
    for id in [401, 402] {
        service
            .upsert_milestone(&ctx, "acme", "widget", milestone_record(id, 7, "twin"))
            .await
            .expect("milestone seed must succeed");
    }

    let router = router_for(service, ctx);

    let mut seen = Vec::new();
    for page in 1..=2 {
        let json = body_json(
            get(
                router.clone(),
                &format!("/repos/acme/widget/milestones?per_page=1&page={page}"),
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
        vec![401, 402],
        "two windows over two equally numbered milestones must return each exactly once"
    );
}

#[tokio::test]
async fn milestones_are_listed_by_number() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_milestone(&ctx, "acme", "widget", milestone_record(2, 2, "v2.0"))
        .await
        .expect("milestone seed must succeed");
    service
        .upsert_milestone(&ctx, "acme", "widget", milestone_record(1, 1, "v1.0"))
        .await
        .expect("milestone seed must succeed");

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/widget/milestones").await;

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let items = json.as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["number"], 1);
    assert_eq!(items[0]["title"], "v1.0");
    assert_eq!(items[1]["number"], 2);
}

#[tokio::test]
async fn milestones_of_unknown_repository_return_404() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/nope/milestones").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn milestones_are_tenant_scoped() {
    let owner = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;
    let service = common::service_over(db.clone(), "https://api.github.com");
    service
        .upsert_repo(&owner, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_milestone(&owner, "acme", "widget", milestone_record(3, 3, "secret"))
        .await
        .expect("milestone seed must succeed");

    let stranger = common::caller_in(Uuid::new_v4());
    let router = router_for(service, stranger);
    let response = get(router, "/repos/acme/widget/milestones").await;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "another tenant must not even learn the repository is mirrored"
    );
}

#[tokio::test]
async fn milestone_upsert_is_idempotent_by_id() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_milestone(&ctx, "acme", "widget", milestone_record(4, 4, "v4.0"))
        .await
        .expect("first upsert must succeed");

    let mut updated = milestone_record(4, 4, "v4.0");
    updated.state = "closed".to_owned();
    updated.closed_issues = 5;
    service
        .upsert_milestone(&ctx, "acme", "widget", updated)
        .await
        .expect("second upsert must succeed");

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/widget/milestones").await;
    let json = body_json(response).await;
    let items = json.as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["state"], "closed");
    assert_eq!(items[0]["closed_issues"], 5);
}
