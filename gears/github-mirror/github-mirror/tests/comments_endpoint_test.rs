#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use github_mirror::api::rest::routes::{ConcreteService, register_routes};
use github_mirror::domain::repo::{CommentRecord, RepoRecord};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

fn repo_record() -> RepoRecord {
    RepoRecord {
        node_id: None,
        id: 4101,
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

fn comment_record(id: i64, issue_number: i64, created_at: &str) -> CommentRecord {
    CommentRecord {
        id,
        repo_id: 0,
        issue_number,
        author_login: Some("frank".to_owned()),
        body: Some("looks good".to_owned()),
        created_at: created_at.to_owned(),
        updated_at: created_at.to_owned(),
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
async fn an_over_long_filter_value_never_reaches_the_link_header() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");

    // This listing ignores `state`, but it still carries the value into its
    // pagination links, so the length has to be bounded before that.
    let huge = "a".repeat(5_000);
    let response = get(
        router_for(service, ctx),
        &format!("/repos/acme/widget/issues/7/comments?state={huge}"),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        response.headers().get(axum::http::header::LINK).is_none(),
        "a refused request must carry no Link header at all"
    );
}

#[tokio::test]
async fn issue_comments_are_listed_oldest_first_for_their_issue() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_comment(
            &ctx,
            "acme",
            "widget",
            comment_record(2, 7, "2026-08-20T00:00:00Z"),
        )
        .await
        .expect("comment seed must succeed");
    service
        .upsert_comment(
            &ctx,
            "acme",
            "widget",
            comment_record(1, 7, "2026-08-18T00:00:00Z"),
        )
        .await
        .expect("comment seed must succeed");
    service
        .upsert_comment(
            &ctx,
            "acme",
            "widget",
            comment_record(3, 8, "2026-08-19T00:00:00Z"),
        )
        .await
        .expect("comment seed must succeed");

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/widget/issues/7/comments").await;

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let items = json.as_array().expect("items");
    assert_eq!(items.len(), 2, "only issue 7 comments must be returned");
    assert_eq!(items[0]["id"], 1);
    assert_eq!(items[1]["id"], 2);
    assert_eq!(items[0]["user"]["login"], "frank");
}

#[tokio::test]
async fn issue_comments_of_unknown_repository_return_404() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/nope/issues/7/comments").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn issue_comments_are_tenant_scoped() {
    let owner = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;
    let service = common::service_over(db.clone(), "https://api.github.com");
    service
        .upsert_repo(&owner, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_comment(
            &owner,
            "acme",
            "widget",
            comment_record(4, 7, "2026-08-20T00:00:00Z"),
        )
        .await
        .expect("comment seed must succeed");

    let stranger = common::caller_in(Uuid::new_v4());
    let router = router_for(service, stranger);
    let response = get(router, "/repos/acme/widget/issues/7/comments").await;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "another tenant must not even learn the repository is mirrored"
    );
}
