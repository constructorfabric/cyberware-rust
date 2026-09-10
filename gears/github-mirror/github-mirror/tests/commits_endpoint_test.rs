#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use github_mirror::api::rest::routes::{ConcreteService, register_routes};
use github_mirror::domain::repo::{CommitRecord, RepoRecord};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

fn repo_record() -> RepoRecord {
    RepoRecord {
        node_id: None,
        id: 900,
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

fn commit_record(sha: &str, committed_at: &str, message: &str) -> CommitRecord {
    CommitRecord {
        repo_id: 0,
        sha: sha.to_owned(),
        message: message.to_owned(),
        author_login: Some("alice".to_owned()),
        committer_login: Some("alice".to_owned()),
        authored_at: Some(committed_at.to_owned()),
        committed_at: Some(committed_at.to_owned()),
        additions: 5,
        deletions: 1,
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
async fn the_since_filter_narrows_the_commit_listing() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    for (sha, committed_at) in [
        ("aaa111", "2026-01-01T00:00:00Z"),
        ("bbb222", "2026-01-01T00:00:01Z"),
        ("ccc333", "2026-01-01T00:00:02Z"),
    ] {
        service
            .upsert_commit(
                &ctx,
                "acme",
                "widget",
                commit_record(sha, committed_at, "message"),
            )
            .await
            .expect("commit seed must succeed");
    }

    let router = router_for(service, ctx);

    let shas = |json: serde_json::Value| -> Vec<String> {
        json.as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["sha"].as_str().map(ToOwned::to_owned))
            .collect()
    };

    let all = body_json(get(router.clone(), "/repos/acme/widget/commits").await).await;
    assert_eq!(shas(all).len(), 3, "no since means every commit");

    let from_the_second = body_json(
        get(
            router.clone(),
            "/repos/acme/widget/commits?since=2026-01-01T00:00:01Z",
        )
        .await,
    )
    .await;
    assert_eq!(
        shas(from_the_second),
        vec!["ccc333".to_owned(), "bbb222".to_owned()],
        "a whole-second bound includes its own second, newest first"
    );

    let fractional = body_json(
        get(
            router,
            "/repos/acme/widget/commits?since=2026-01-01T00:00:00.500Z",
        )
        .await,
    )
    .await;
    assert_eq!(
        shas(fractional),
        vec!["ccc333".to_owned(), "bbb222".to_owned()],
        "the same rounding-up boundary as issues and pull requests"
    );
}

#[tokio::test]
async fn commits_are_listed_newest_first() {
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
            commit_record("aaa111", "2026-08-18T00:00:00Z", "older"),
        )
        .await
        .expect("commit seed must succeed");
    service
        .upsert_commit(
            &ctx,
            "acme",
            "widget",
            commit_record("bbb222", "2026-08-20T00:00:00Z", "newer"),
        )
        .await
        .expect("commit seed must succeed");

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/widget/commits").await;

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let items = json.as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["sha"], "bbb222");
    assert_eq!(items[0]["commit"]["message"], "newer");
    assert_eq!(items[1]["sha"], "aaa111");
}

#[tokio::test]
async fn commits_of_unknown_repository_return_404() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/nope/commits").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn commits_are_tenant_scoped() {
    let owner = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;
    let service = common::service_over(db.clone(), "https://api.github.com");
    service
        .upsert_repo(&owner, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_commit(
            &owner,
            "acme",
            "widget",
            commit_record("ccc333", "2026-08-20T00:00:00Z", "secret"),
        )
        .await
        .expect("commit seed must succeed");

    let stranger = common::caller_in(Uuid::new_v4());
    let router = router_for(service, stranger);
    let response = get(router, "/repos/acme/widget/commits").await;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "another tenant must not even learn the repository is mirrored"
    );
}

#[tokio::test]
async fn commit_upsert_is_idempotent_by_sha() {
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
            commit_record("ddd444", "2026-08-20T00:00:00Z", "first message"),
        )
        .await
        .expect("first upsert must succeed");

    let mut updated = commit_record("ddd444", "2026-08-20T00:00:00Z", "amended message");
    updated.additions = 50;
    service
        .upsert_commit(&ctx, "acme", "widget", updated)
        .await
        .expect("second upsert must succeed");

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/widget/commits").await;
    let json = body_json(response).await;
    let items = json.as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["commit"]["message"], "amended message");
    assert_eq!(items[0]["author"]["login"], "alice");
}
