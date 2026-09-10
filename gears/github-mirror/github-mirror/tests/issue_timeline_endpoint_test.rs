#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use github_mirror::api::rest::routes::{ConcreteService, register_routes};
use github_mirror::domain::repo::{IssueTimelineEventRecord, RepoRecord};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

fn repo_record() -> RepoRecord {
    RepoRecord {
        node_id: None,
        id: 9301,
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

fn timeline_record(
    issue_number: i64,
    position: i64,
    event: &str,
    payload_json: &str,
) -> IssueTimelineEventRecord {
    IssueTimelineEventRecord {
        repo_id: 0,
        issue_number,
        position,
        event: event.to_owned(),
        created_at: Some("2026-08-20T00:00:00Z".to_owned()),
        actor_login: Some("kate".to_owned()),
        payload_json: payload_json.to_owned(),
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
async fn timeline_entries_are_replayed_in_order_with_their_own_payloads() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    for entry in [
        timeline_record(
            7,
            1,
            "committed",
            r#"{"event":"committed","sha":"c1","message":"fix it"}"#,
        ),
        timeline_record(
            7,
            0,
            "labeled",
            r#"{"event":"labeled","label":{"name":"bug"},"actor":{"login":"kate"}}"#,
        ),
        timeline_record(8, 0, "closed", r#"{"event":"closed"}"#),
    ] {
        service
            .upsert_issue_timeline_event(&ctx, "acme", "widget", entry)
            .await
            .expect("timeline seed must succeed");
    }

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/widget/issues/7/timeline").await;

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    let items = json.as_array().expect("items");
    assert_eq!(items.len(), 2, "only issue 7 entries must be returned");

    assert_eq!(items[0]["event"], "labeled", "position order must hold");
    assert_eq!(
        items[0]["label"]["name"], "bug",
        "the entry's own payload must come back verbatim"
    );
    assert_eq!(items[0]["actor"]["login"], "kate");

    assert_eq!(items[1]["event"], "committed");
    assert_eq!(items[1]["sha"], "c1");
    assert!(
        items[1].get("label").is_none(),
        "one event type's fields must not leak into another"
    );
}

#[tokio::test]
async fn an_arbitrary_timeline_payload_survives_the_round_trip() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");

    let stored = serde_json::json!({
        "event": "cross-referenced",
        "source": { "type": "issue", "issue": { "number": 41, "title": "other" } },
        "unmodeled_field": ["a", 1, null],
        "nested": { "deep": { "flag": true } },
    });
    service
        .upsert_issue_timeline_event(
            &ctx,
            "acme",
            "widget",
            timeline_record(9, 0, "cross-referenced", &stored.to_string()),
        )
        .await
        .expect("timeline seed must succeed");

    let router = router_for(service, ctx);
    let json = body_json(get(router, "/repos/acme/widget/issues/9/timeline").await).await;
    let items = json.as_array().expect("items");

    assert_eq!(
        items[0], stored,
        "every key GitHub sent must come back untouched"
    );
}

#[tokio::test]
async fn timeline_of_unknown_repository_returns_404() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;

    let router = router_for(service, ctx);
    let response = get(router, "/repos/acme/nope/issues/7/timeline").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn timeline_is_tenant_scoped() {
    let owner = common::caller_in(Uuid::new_v4());
    let db = common::inmem_db().await;
    let service = common::service_over(db.clone(), "https://api.github.com");
    service
        .upsert_repo(&owner, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_issue_timeline_event(
            &owner,
            "acme",
            "widget",
            timeline_record(7, 0, "closed", r#"{"event":"closed"}"#),
        )
        .await
        .expect("timeline seed must succeed");

    let stranger = common::caller_in(Uuid::new_v4());
    let router = router_for(service, stranger);
    let response = get(router, "/repos/acme/widget/issues/7/timeline").await;

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "another tenant must not even learn the repository is mirrored"
    );
}

#[tokio::test]
async fn timeline_upsert_is_idempotent_by_position() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_issue_timeline_event(
            &ctx,
            "acme",
            "widget",
            timeline_record(7, 0, "labeled", r#"{"event":"labeled"}"#),
        )
        .await
        .expect("first upsert must succeed");
    service
        .upsert_issue_timeline_event(
            &ctx,
            "acme",
            "widget",
            timeline_record(7, 0, "unlabeled", r#"{"event":"unlabeled"}"#),
        )
        .await
        .expect("second upsert must succeed");

    let router = router_for(service, ctx);
    let json = body_json(get(router, "/repos/acme/widget/issues/7/timeline").await).await;
    let items = json.as_array().expect("items");
    assert_eq!(items.len(), 1, "the same position must not duplicate");
    assert_eq!(items[0]["event"], "unlabeled");
}

#[tokio::test]
async fn an_unparsable_stored_payload_still_serves_its_event() {
    let ctx = common::caller_in(Uuid::new_v4());
    let service = common::service("https://api.github.com").await;
    service
        .upsert_repo(&ctx, repo_record())
        .await
        .expect("repo seed must succeed");
    service
        .upsert_issue_timeline_event(
            &ctx,
            "acme",
            "widget",
            timeline_record(7, 0, "renamed", "not json at all"),
        )
        .await
        .expect("seed must succeed");

    let router = router_for(service, ctx);
    let json = body_json(get(router, "/repos/acme/widget/issues/7/timeline").await).await;
    let items = json.as_array().expect("items");
    assert_eq!(
        items.len(),
        1,
        "one unreadable row must not fail the listing"
    );
    assert_eq!(
        items[0],
        serde_json::json!({ "event": "renamed" }),
        "the documented fallback is the event name alone"
    );
}
