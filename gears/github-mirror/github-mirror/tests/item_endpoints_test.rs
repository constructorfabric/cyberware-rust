#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use github_mirror::api::rest::routes::{ConcreteService, register_routes};
use github_mirror::domain::repo::{
    CommitFileRecord, CommitRecord, IssueRecord, PullRequestRecord, RepoRecord,
};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

fn repo_record() -> RepoRecord {
    RepoRecord {
        node_id: None,
        id: 3001,
        owner: "acme".to_owned(),
        name: "widget".to_owned(),
        full_name: "acme/widget".to_owned(),
        default_branch: "main".to_owned(),
        private: false,
        pushed_at: Some("2026-08-20T00:00:00Z".to_owned()),
        stars: 42,
        forks: 7,
        description: Some("gadgets".to_owned()),
        clone_url: None,
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
async fn get_repo_returns_github_shape() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/widget").await;

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["id"], 3001);
    assert_eq!(json["full_name"], "acme/widget");
    assert_eq!(json["owner"]["login"], "acme");
    assert_eq!(json["stargazers_count"], 42);
    assert_eq!(json["forks_count"], 7);
    assert_eq!(json["default_branch"], "main");
}

#[tokio::test]
async fn get_repo_of_unknown_repository_returns_404() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/nope").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_repo_is_tenant_scoped() {
    let owner = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;
    let service = common::service_over(db.clone(), "https://api.github.com");
    service
        .upsert_repo(&owner, repo_record())
        .await
        .expect("repo seed must succeed");

    let stranger = common::caller_in(Uuid::new_v4());
    let router = router_for(service, stranger);
    let response = get(router, "/repos/acme/widget").await;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "another tenant must not even learn the repository is mirrored"
    );
}

#[tokio::test]
async fn get_issue_returns_the_matching_number() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_issue(
            &ctx,
            "acme",
            "widget",
            IssueRecord {
                author_login: Some("alice".to_owned()),
                author_json: None,
                assignees_json: None,
                labels_json: None,
                comments_count: None,
                locked: None,
                node_id: None,
                id: 1,
                repo_id: 0,
                number: 11,
                title: "an issue".to_owned(),
                body: None,
                state: "open".to_owned(),
                is_pull_request: false,
                created_at: "2026-08-20T00:00:00Z".to_owned(),
                updated_at: "2026-08-20T00:00:00Z".to_owned(),
                closed_at: None,
                html_url: None,
            },
        )
        .await
        .expect("issue seed must succeed");

    let router = router_for(service, ctx);
    let response = get(router.clone(), "/repos/acme/widget/issues/11").await;
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["number"], 11);
    assert_eq!(json["title"], "an issue");

    let missing = get(router, "/repos/acme/widget/issues/999").await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_pull_request_returns_the_matching_number() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_pull_request(
            &ctx,
            "acme",
            "widget",
            PullRequestRecord {
                author_login: Some("alice".to_owned()),
                author_json: None,
                assignees_json: None,
                labels_json: None,
                comments_count: None,
                locked: None,
                requested_reviewers_json: None,
                node_id: None,
                id: 2,
                repo_id: 0,
                number: 12,
                title: "a pr".to_owned(),
                body: None,
                state: "closed".to_owned(),
                draft: false,
                merged: true,
                head_sha: Some("h1".to_owned()),
                base_sha: Some("b1".to_owned()),
                lines_added: 10,
                lines_removed: 2,
                created_at: "2026-08-20T00:00:00Z".to_owned(),
                updated_at: "2026-08-20T00:00:00Z".to_owned(),
                closed_at: Some("2026-08-21T00:00:00Z".to_owned()),
                merged_at: Some("2026-08-21T00:00:00Z".to_owned()),
                html_url: Some("https://github.com/acme/widget/pull/12".to_owned()),
                head_ref: Some("feature".to_owned()),
                base_ref: Some("main".to_owned()),
            },
        )
        .await
        .expect("pull seed must succeed");

    let router = router_for(service, ctx);
    let response = get(router.clone(), "/repos/acme/widget/pulls/12").await;
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["number"], 12);
    assert_eq!(json["merged"], true);
    assert_eq!(json["head"]["sha"], "h1");
    assert_eq!(json["additions"], 10);

    let missing = get(router, "/repos/acme/widget/pulls/999").await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_commit_returns_stats_and_files() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_commit(
            &ctx,
            "acme",
            "widget",
            CommitRecord {
                repo_id: 0,
                sha: "aaa".to_owned(),
                message: "first".to_owned(),
                author_login: Some("alice".to_owned()),
                committer_login: Some("bob".to_owned()),
                authored_at: Some("2026-08-20T00:00:00Z".to_owned()),
                committed_at: Some("2026-08-20T00:00:00Z".to_owned()),
                additions: 4,
                deletions: 1,
            },
        )
        .await
        .expect("commit seed must succeed");
    service
        .upsert_commit_file(
            &ctx,
            "acme",
            "widget",
            CommitFileRecord {
                repo_id: 0,
                commit_sha: "aaa".to_owned(),
                filename: "src/lib.rs".to_owned(),
                status: "modified".to_owned(),
                additions: 4,
                deletions: 1,
                changes: 5,
                previous_filename: None,
                sha: Some("blob1".to_owned()),
            },
        )
        .await
        .expect("commit file seed must succeed");

    let router = router_for(service, ctx);
    let response = get(router.clone(), "/repos/acme/widget/commits/aaa").await;
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["sha"], "aaa");
    assert_eq!(json["commit"]["message"], "first");
    assert_eq!(json["author"]["login"], "alice");
    assert_eq!(json["stats"]["additions"], 4);
    assert_eq!(json["stats"]["total"], 5);
    assert_eq!(json["files"][0]["filename"], "src/lib.rs");

    let missing = get(router, "/repos/acme/widget/commits/bbb").await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}
