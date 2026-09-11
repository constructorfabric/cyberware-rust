//! Smoke tests for the harness/`test_router` wiring itself - every route
//! registered without a matchit conflict, and one request per resource
//! group round-trips. Per-endpoint Hard-Error-Catalog coverage lands with
//! task group 11's dedicated test files; this just de-risks the router
//! build (a duplicate-route conflict panics at router-build time, not
//! compile time).

use crate::test_support::EventBrokerHarness;

#[tokio::test]
async fn harness_builds_and_topics_round_trip() {
    let harness = EventBrokerHarness::builder().build().await;
    let page: serde_json::Value = harness.api_v1().get_topics().expect_ok().await;
    assert_eq!(page["items"], serde_json::json!([]));
}

#[tokio::test]
async fn producer_register_round_trips() {
    let harness = EventBrokerHarness::builder().build().await;
    let resp = harness
        .api_v1()
        .post_producers()
        .with_body(crate::test_support::Json(serde_json::json!({
            "mode": "chained",
            "client_agent": "test-agent",
        })))
        .send()
        .await;
    resp.assert_status(201);
    let body = resp.json();
    assert_eq!(body["mode"], "chained");
    assert_eq!(body["client_agent"], "test-agent");
}

#[tokio::test]
async fn consumer_group_create_then_delete() {
    let harness = EventBrokerHarness::builder().build().await;
    let create = harness.api_v1().post_consumer_groups().send().await;
    create.assert_status(201);
    let id = create.json()["id"].as_str().unwrap().to_owned();

    let delete = harness.api_v1().delete_consumer_group(&id).send().await;
    delete.assert_status(204);
}

#[tokio::test]
async fn subscription_join_requires_existing_consumer_group() {
    let harness = EventBrokerHarness::builder().build().await;
    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(crate::test_support::Json(serde_json::json!({
            "consumer_group": "gts.cf.core.events.consumer_group.v1~x.eb.t1.missing.v1",
            "client_agent": "test-agent",
            "interests": [],
        })))
        .send()
        .await;
    resp.assert_status(404);
}

#[tokio::test]
async fn events_publish_requires_a_registered_event_type() {
    let harness = EventBrokerHarness::builder().build().await;
    let resp = harness
        .api_v1()
        .post_events()
        .with_body(crate::test_support::Json(serde_json::json!({
            "id": uuid::Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": uuid::Uuid::new_v4(),
            "source": "test",
            "subject": "s1",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": chrono::Utc::now().to_rfc3339(),
        })))
        .send()
        .await;
    resp.assert_status(404);
}
