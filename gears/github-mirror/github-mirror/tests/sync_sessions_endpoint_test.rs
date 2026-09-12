#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use github_mirror::api::rest::routes::{ConcreteService, register_routes};
use github_mirror::domain::ports::github::{FetchedRepository, ListingCompleteness};
use github_mirror::domain::repo::RepoRecord;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

/// The smallest fetch result the sync accepts: one repository, nothing else.
fn fetched() -> FetchedRepository {
    FetchedRepository {
        complete: ListingCompleteness::all_complete(),
        repository: RepoRecord {
            id: 42,
            node_id: None,
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
        },
        issues: vec![],
        pull_requests: vec![],
        commits: vec![],
        comments: vec![],
        review_comments: vec![],
        reviews: vec![],
        labels: vec![],
        milestones: vec![],
        releases: vec![],
        branches: vec![],
        contributors: vec![],
        workflow_runs: vec![],
        pull_request_files: vec![],
        tags: vec![],
        commit_files: vec![],
        review_threads: vec![],
        commit_comments: vec![],
        issue_events: vec![],
        deployments: vec![],
        pull_request_commits: vec![],
        commit_statuses: vec![],
        workflow_jobs: vec![],
        issue_reactions: vec![],
        check_runs: vec![],
        issue_timeline: vec![],
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

async fn send(router: Router, method: Method, uri: &str) -> axum::http::Response<Body> {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    router.oneshot(request).await.unwrap()
}

#[tokio::test]
async fn a_sync_is_queued_first_and_only_succeeds_once_the_worker_runs_it() {
    let ctx = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;
    let service = common::service_with_github(
        db,
        "https://api.github.com",
        Arc::new(common::FakeGithub {
            result: Some(fetched()),
        }),
    );
    let mut pump = common::SyncPump::take(&service).await;

    let router = router_for(service.clone(), ctx);
    let response = send(
        router.clone(),
        Method::POST,
        "/github-mirror/v1/repos/acme/widget/sync",
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let accepted = body_json(response).await;
    let session_id = accepted["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    assert_eq!(accepted["repository"], "acme/widget");
    assert_eq!(accepted["status"], "queued");

    let uri = format!("/github-mirror/v1/sessions/{session_id}");
    let queued = body_json(send(router.clone(), Method::GET, &uri).await).await;
    assert_eq!(
        queued["status"], "queued",
        "the session is durable before the work starts"
    );
    assert_eq!(queued["progress_percent"], 0);
    assert!(queued["started_at"].is_null());
    assert!(queued["summary"].is_null());
    assert!(queued["duration_ms"].is_null());

    assert_eq!(pump.drain(&service).await, 1);

    let session = body_json(send(router.clone(), Method::GET, &uri).await).await;
    assert_eq!(session["id"], session_id.as_str());
    assert_eq!(session["repository"], "acme/widget");
    assert_eq!(session["status"], "complete");
    assert_eq!(session["progress_percent"], 100);
    assert!(session["error"].is_null());
    assert_eq!(session["summary"]["repository"], "acme/widget");
    assert!(session["started_at"].is_string());
    assert!(session["ended_at"].is_string());
    assert!(
        session["duration_ms"].as_i64().expect("duration_ms") >= 0,
        "duration comes from started_at/ended_at"
    );

    let listed = body_json(send(router, Method::GET, "/github-mirror/v1/sessions").await).await;
    assert_eq!(listed["items"].as_array().expect("items").len(), 1);
    assert_eq!(listed["items"][0]["id"], session_id.as_str());
}

#[tokio::test]
async fn a_failed_sync_leaves_a_failed_session_behind() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    let mut pump = common::SyncPump::take(&service).await;

    let router = router_for(service.clone(), ctx);
    let response = send(
        router.clone(),
        Method::POST,
        "/github-mirror/v1/repos/acme/nope/sync",
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "nobody is waiting on the fetch, so the request cannot report its failure"
    );
    pump.drain(&service).await;

    let listed = body_json(send(router, Method::GET, "/github-mirror/v1/sessions").await).await;
    let items = listed["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "the failed run must still be recorded");
    assert_eq!(items[0]["status"], "failed");
    assert_eq!(items[0]["repository"], "acme/nope");
    assert!(items[0]["error"].is_string());
    assert!(items[0]["summary"].is_null());
}

#[tokio::test]
async fn a_restart_closes_out_sessions_left_in_flight() {
    let ctx = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;
    let service = common::service_with_github(
        db,
        "https://api.github.com",
        Arc::new(common::FakeGithub {
            result: Some(fetched()),
        }),
    );

    // No pump: the job is queued and nothing ever runs it, which is exactly
    // the state a process leaves behind when it dies mid-sync.
    let router = router_for(service.clone(), ctx);
    let session_id = body_json(
        send(
            router.clone(),
            Method::POST,
            "/github-mirror/v1/repos/acme/widget/sync",
        )
        .await,
    )
    .await["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();

    let swept = service
        .sweep_interrupted_sessions()
        .await
        .expect("the sweep must succeed");
    assert_eq!(swept, 1);

    let session = body_json(
        send(
            router,
            Method::GET,
            &format!("/github-mirror/v1/sessions/{session_id}"),
        )
        .await,
    )
    .await;
    assert_eq!(session["status"], "interrupted");
    assert!(session["error"].is_string());
    assert!(session["ended_at"].is_string());
}

#[tokio::test]
async fn an_unknown_session_id_is_404() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;

    let router = router_for(service, ctx);
    let response = send(
        router,
        Method::GET,
        &format!("/github-mirror/v1/sessions/{}", Uuid::new_v4()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sessions_are_tenant_scoped() {
    let db = common::inmem_db().await;
    let service = common::service_with_github(
        db,
        "https://api.github.com",
        Arc::new(common::FakeGithub {
            result: Some(fetched()),
        }),
    );

    let owner = common::caller_in(Uuid::new_v4());
    let owner_router = router_for(service.clone(), owner);
    let summary = body_json(
        send(
            owner_router,
            Method::POST,
            "/github-mirror/v1/repos/acme/widget/sync",
        )
        .await,
    )
    .await;
    let session_id = summary["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();

    let stranger = common::caller_in(Uuid::new_v4());
    let stranger_router = router_for(service, stranger);
    let response = send(
        stranger_router.clone(),
        Method::GET,
        &format!("/github-mirror/v1/sessions/{session_id}"),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "another tenant must not even learn the session exists"
    );
    let listed =
        body_json(send(stranger_router, Method::GET, "/github-mirror/v1/sessions").await).await;
    assert!(listed["items"].as_array().expect("items").is_empty());
}
