#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use github_mirror::api::rest::routes::{ConcreteService, register_routes};
use github_mirror::domain::repo::{RepoRecord, WorkflowJobRecord};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

fn repo_record() -> RepoRecord {
    RepoRecord {
        node_id: None,
        id: 9001,
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

fn job_record(id: i64, run_id: i64, name: &str, steps_json: Option<&str>) -> WorkflowJobRecord {
    WorkflowJobRecord {
        id,
        repo_id: 0,
        run_id,
        run_attempt: 1,
        name: name.to_owned(),
        status: Some("completed".to_owned()),
        conclusion: Some("success".to_owned()),
        head_sha: "c1".to_owned(),
        runner_name: Some("ubuntu-latest".to_owned()),
        started_at: Some("2026-08-20T00:00:00Z".to_owned()),
        completed_at: Some("2026-08-20T00:05:00Z".to_owned()),
        html_url: Some(format!(
            "https://github.com/acme/widget/actions/runs/{run_id}/job/{id}"
        )),
        steps_json: steps_json.map(ToOwned::to_owned),
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
async fn workflow_jobs_are_listed_for_their_run_in_the_github_envelope() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    for job in [
        job_record(2, 70, "build", Some(r#"[{"name":"Checkout","number":1}]"#)),
        job_record(1, 70, "lint", None),
        job_record(3, 71, "other-run", None),
    ] {
        service
            .upsert_workflow_job(&ctx, "acme", "widget", job)
            .await
            .expect("job seed must succeed");
    }

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/widget/actions/runs/70/jobs").await;

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["total_count"], 2);
    let jobs = json["jobs"].as_array().expect("jobs");
    assert_eq!(jobs.len(), 2, "only run 70 jobs must be returned");
    assert_eq!(jobs[0]["id"], 1, "jobs must come back in job-id order");
    assert_eq!(jobs[1]["id"], 2);
    assert_eq!(jobs[1]["name"], "build");
    assert_eq!(jobs[1]["runner_name"], "ubuntu-latest");
    assert_eq!(jobs[1]["steps"][0]["name"], "Checkout");
    assert_eq!(
        jobs[0]["steps"],
        serde_json::json!([]),
        "a job stored without steps must still serve an array"
    );
}

#[tokio::test]
async fn step_fields_outside_the_typed_ones_still_reach_the_caller() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_workflow_job(
            &ctx,
            "acme",
            "widget",
            job_record(
                4,
                72,
                "build",
                Some(
                    r#"[{"name":"Checkout","number":1,"status":"completed","conclusion":"success","unmodeled":{"id":7},"duration_ms":1234}]"#,
                ),
            ),
        )
        .await
        .expect("job seed must succeed");

    let router = router_for(service, ctx);
    let json = body_json(get(router, "/repos/acme/widget/actions/runs/72/jobs").await).await;
    let step = &json["jobs"][0]["steps"][0];

    assert_eq!(step["name"], "Checkout", "{step:?}");
    assert_eq!(step["number"], 1);
    assert_eq!(step["conclusion"], "success");
    assert_eq!(
        step["unmodeled"]["id"], 7,
        "a field the mirror does not model must survive: {step:?}"
    );
    assert_eq!(step["duration_ms"], 1234, "{step:?}");
}

#[tokio::test]
async fn workflow_jobs_of_unknown_repository_return_404() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/nope/actions/runs/70/jobs").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn workflow_jobs_are_tenant_scoped() {
    let owner = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;
    let service = common::service_over(db.clone(), "https://api.github.com");
    service
        .upsert_repo(&owner, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_workflow_job(&owner, "acme", "widget", job_record(4, 70, "secret", None))
        .await
        .expect("job seed must succeed");

    let stranger = common::caller_in(Uuid::new_v4());
    let router = router_for(service, stranger);
    let response = get(router, "/repos/acme/widget/actions/runs/70/jobs").await;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "another tenant must not even learn the repository is mirrored"
    );
}

#[tokio::test]
async fn workflow_job_upsert_is_idempotent_by_id() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_workflow_job(&ctx, "acme", "widget", job_record(5, 70, "build", None))
        .await
        .expect("first upsert must succeed");

    let mut updated = job_record(5, 70, "build", None);
    updated.status = Some("in_progress".to_owned());
    updated.conclusion = None;
    service
        .upsert_workflow_job(&ctx, "acme", "widget", updated)
        .await
        .expect("second upsert must succeed");

    let router = router_for(service, ctx);
    let json = body_json(get(router, "/repos/acme/widget/actions/runs/70/jobs").await).await;
    assert_eq!(json["total_count"], 1);
    let jobs = json["jobs"].as_array().expect("jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["status"], "in_progress");
    assert!(jobs[0]["conclusion"].is_null());
}
