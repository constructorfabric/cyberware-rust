//! `handlers/delivery/streaming.rs` coverage (`eb-rest-handlers` task 11.5):
//! `:stream`/`:sse` frame shapes and streaming-in-progress lifecycle,
//! split out from `subscriptions_tests.rs` (item #12 of `REVIEW-TODO.md`
//! - this file was originally just a section of that one).
//!
//! These tests bypass `RequestCase::send()` (which eagerly collects the
//! *entire* response body via `axum::body::to_bytes` - fine for ordinary
//! JSON responses, but `:stream`/`:sse` bodies are intentionally long-lived
//! and never naturally end, so collecting them in full would hang forever).
//! Instead they drive `harness.router()` directly with `tower::ServiceExt`,
//! matching `api/rest/routes/routes_tests.rs`'s existing streaming-test
//! pattern (`idle_timeout_closes_connection_after_silence_but_not_between_heartbeats`).

use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use serde_json::json;
use tokio_stream::StreamExt;
use tower::ServiceExt;
use uuid::Uuid;

use crate::test_support::{EventBrokerHarness, Json, StaticTypesRegistry};

#[tokio::test]
async fn stream_events_returns_409_positions_not_set_when_unseeded() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "a",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = harness.api_v1().get_events_stream(&id).send().await;

    resp.assert_status(409);
    let seek_hint = format!(
        "call POST /v1/subscriptions/{}:seek before re-opening the stream",
        id
    );
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
            "title": "Failed Precondition",
            "status": 409,
            "detail": "Operation precondition not met",
            "instance": "/event-broker/v1/events:stream",
            "context": {
                "violations": [{
                    "type": "cursor_missing",
                    "subject": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1:0",
                    "description": seek_hint,
                }],
                "resource_type": "gts.cf.core.events.subscription.v1~",
            },
        })
    );
}

#[tokio::test]
async fn stream_events_returns_404_for_unknown_subscription() {
    let harness = EventBrokerHarness::builder().build().await;
    let missing_id = Uuid::new_v4();

    let resp = harness
        .api_v1()
        .get_events_stream(&missing_id.to_string())
        .send()
        .await;

    resp.assert_status(404);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": format!("subscription '{missing_id}' does not exist"),
            "instance": "/event-broker/v1/events:stream",
            "context": {
                "resource_name": missing_id.to_string(),
                "resource_type": "gts.cf.core.events.subscription.v1~",
            },
        })
    );
}

#[tokio::test]
async fn stream_events_happy_path_emits_topology_frame_first() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "a",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0, "value": "earliest" }]
        })))
        .send()
        .await
        .assert_status(200);

    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/event-broker/v1/events:stream?subscription_id={id}"
        ))
        .body(Body::empty())
        .expect("request must build");
    let response = harness
        .router()
        .clone()
        .oneshot(request)
        .await
        .expect("the service itself must not error");
    assert_eq!(response.status(), StatusCode::OK);

    // Extract the per-connection boundary from the Content-Type header so the
    // assertion is not coupled to the exact UUID value.
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .expect("Content-Type must carry a boundary parameter")
        .trim()
        .to_owned();

    let mut stream = response.into_body().into_data_stream();
    let first_chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("must receive the first frame within 2s")
        .expect("stream must not end before the first frame")
        .expect("first chunk must not be an error");
    let text = String::from_utf8_lossy(&first_chunk).into_owned();
    assert_eq!(
        text,
        format!(
            "--{boundary}\r\nContent-Type: application/json\r\n\r\n\
             {{\"kind\":\"topology\",\"topology_version\":1,\"assigned\":\
             [{{\"topic\":\"gts.cf.core.events.topic.v1~x.eb.t1.topic.v1\",\"partition\":0,\"offset\":0,\
             \"last_examined\":0}}]}}\r\n"
        )
    );
}

#[tokio::test]
async fn stream_events_second_stream_on_same_subscription_returns_409_streaming_in_progress() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "a",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0, "value": "earliest" }]
        })))
        .send()
        .await
        .assert_status(200);

    let request1 = Request::builder()
        .method("GET")
        .uri(format!(
            "/event-broker/v1/events:stream?subscription_id={id}"
        ))
        .body(Body::empty())
        .expect("request must build");
    // Held alive (not dropped) for the rest of the test - dropping the
    // response body would clear the active-stream marker immediately.
    let first_response = harness
        .router()
        .clone()
        .oneshot(request1)
        .await
        .expect("the service itself must not error");
    assert_eq!(first_response.status(), StatusCode::OK);

    let resp = harness.api_v1().get_events_stream(&id).send().await;

    resp.assert_status(409);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
            "title": "Failed Precondition",
            "status": 409,
            "detail": "Operation precondition not met",
            "instance": "/event-broker/v1/events:stream",
            "context": {
                "violations": [{
                    "type": "streaming_in_progress",
                    "subject": id,
                    "description": "one stream per subscription; DELETE is the only permitted concurrent call",
                }],
                "resource_type": "gts.cf.core.events.subscription.v1~",
            },
        })
    );

    drop(first_response);
}

#[tokio::test]
async fn seek_while_streaming_returns_409_streaming_in_progress() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "a",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0, "value": "earliest" }]
        })))
        .send()
        .await
        .assert_status(200);

    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/event-broker/v1/events:stream?subscription_id={id}"
        ))
        .body(Body::empty())
        .expect("request must build");
    let stream_response = harness
        .router()
        .clone()
        .oneshot(request)
        .await
        .expect("the service itself must not error");
    assert_eq!(stream_response.status(), StatusCode::OK);

    let resp = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0, "value": "latest" }]
        })))
        .send()
        .await;

    resp.assert_status(409);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
            "title": "Failed Precondition",
            "status": 409,
            "detail": "Operation precondition not met",
            "instance": format!("/event-broker/v1/subscriptions/{id}:seek"),
            "context": {
                "violations": [{
                    "type": "streaming_in_progress",
                    "subject": id,
                    "description": "one stream per subscription; DELETE is the only permitted concurrent call",
                }],
                "resource_type": "gts.cf.core.events.subscription.v1~",
            },
        })
    );

    drop(stream_response);
}

#[tokio::test]
async fn stream_events_delivers_published_event_via_notification_wakeup() {
    // Verifies the D6 wake-up mechanism itself (`domain::notify::DeliveryNotifier`,
    // `ClusterCacheV1::watch_prefix`) actually delivers a live event promptly -
    // not just that a pre-seeded position can be replayed. Group 7 removed the
    // old fixed-interval poll, so a regression here would only show up as a
    // stream that never wakes until its next heartbeat.
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
        ])))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tenant_id = Uuid::new_v4();
    let id = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "a",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": tenant_id,
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0, "value": "earliest" }]
        })))
        .send()
        .await
        .assert_status(200);

    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/event-broker/v1/events:stream?subscription_id={id}"
        ))
        .body(Body::empty())
        .expect("request must build");
    let response = harness
        .router()
        .clone()
        .oneshot(request)
        .await
        .expect("the service itself must not error");
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("must receive the topology frame within 2s")
        .expect("stream must not end before the topology frame")
        .expect("topology chunk must not be an error");

    let event_id = Uuid::new_v4();
    harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": event_id,
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": tenant_id,
            "source": "test-stream_events_delivers_published_event_via_notification_wakeup",
            "subject": "s1",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": chrono::Utc::now().to_rfc3339(),
        })))
        .send()
        .await
        .assert_status(202);

    let event_chunk = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
        .await
        .expect(
            "the notification wake-up must deliver the event within 3s, not wait for a heartbeat",
        )
        .expect("stream must not end before the event frame")
        .expect("event chunk must not be an error");
    let text = String::from_utf8_lossy(&event_chunk).into_owned();
    let json_start = text
        .find("\r\n\r\n")
        .expect("frame must have a header/body separator")
        + 4;
    let json_body = text[json_start..].trim_end_matches("\r\n");
    let frame: serde_json::Value =
        serde_json::from_str(json_body).expect("frame body must be valid JSON");
    assert_eq!(frame["kind"], "event");
    assert_eq!(frame["payload"]["id"], event_id.to_string());
    assert_eq!(
        frame["payload"]["type"],
        "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"
    );
    assert_eq!(
        frame["payload"]["topic"],
        "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1"
    );
    assert_eq!(frame["payload"]["tenant_id"], tenant_id.to_string());
    assert_eq!(frame["payload"]["subject"], "s1");
}

#[tokio::test]
async fn sse_events_returns_404_for_unknown_subscription() {
    let harness = EventBrokerHarness::builder().build().await;
    let missing_id = Uuid::new_v4();

    let resp = harness
        .api_v1()
        .get_events_sse(&missing_id.to_string())
        .send()
        .await;

    resp.assert_status(404);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": format!("subscription '{missing_id}' does not exist"),
            "instance": "/event-broker/v1/events:sse",
            "context": {
                "resource_name": missing_id.to_string(),
                "resource_type": "gts.cf.core.events.subscription.v1~",
            },
        })
    );
}

#[tokio::test]
async fn sse_events_happy_path_opens_a_text_event_stream() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "a",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0, "value": "earliest" }]
        })))
        .send()
        .await
        .assert_status(200);

    let request = Request::builder()
        .method("GET")
        .uri(format!("/event-broker/v1/events:sse?subscription_id={id}"))
        .body(Body::empty())
        .expect("request must build");
    let response = harness
        .router()
        .clone()
        .oneshot(request)
        .await
        .expect("the service itself must not error");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap()),
        Some("text/event-stream")
    );

    let mut stream = response.into_body().into_data_stream();
    let first_chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("must receive the first event within 2s")
        .expect("stream must not end before the first event")
        .expect("first chunk must not be an error");
    let text = String::from_utf8_lossy(&first_chunk).into_owned();
    assert_eq!(
        text,
        "event: topology\ndata: {\"kind\":\"topology\",\"topology_version\":1,\"assigned\":\
         [{\"topic\":\"gts.cf.core.events.topic.v1~x.eb.t1.topic.v1\",\"partition\":0,\"offset\":0,\
         \"last_examined\":0}]}\n\n"
    );
}

// scenario: consumer/stream/1.07-guardrail-stream-accept-json-rejected.md
#[tokio::test]
async fn stream_events_returns_406_when_accept_is_application_json() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .get_events_stream("00000000-0000-0000-0000-000000000000")
        .with_header(
            axum::http::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        )
        .send()
        .await;

    resp.assert_status(406);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 406,
            "detail": "this endpoint serves multipart/mixed only",
            "instance": "/event-broker/v1/events:stream",
            "context": {
                "format": "this endpoint serves multipart/mixed only",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

// scenario: consumer/stream/1.08-guardrail-sse-from-stream-endpoint.md
#[tokio::test]
async fn stream_events_returns_406_when_accept_is_text_event_stream() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .get_events_stream("00000000-0000-0000-0000-000000000000")
        .with_header(
            axum::http::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        )
        .send()
        .await;

    resp.assert_status(406);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 406,
            "detail": "SSE is served only at /v1/events:sse; this endpoint serves multipart/mixed",
            "instance": "/event-broker/v1/events:stream",
            "context": {
                "format": "SSE is served only at /v1/events:sse; this endpoint serves multipart/mixed",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

/// Open a stream for sub_a, then JOIN sub_b into the same group.
/// The coordinator must push a `Frame::Topology` into sub_a's open stream.
#[tokio::test]
async fn second_join_pushes_topology_frame_to_existing_stream() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 4 },
        ])))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tenant_id = Uuid::new_v4();

    // sub_a JOINs and gets all 4 partitions.
    let id_a = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "a",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": tenant_id,
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness
        .api_v1()
        .post_subscription_seek(&id_a)
        .with_body(Json(&json!({
            "partition_positions": [
                { "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0, "value": "earliest" },
                { "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 1, "value": "earliest" },
                { "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 2, "value": "earliest" },
                { "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 3, "value": "earliest" },
            ]
        })))
        .send()
        .await
        .assert_status(200);

    // Open sub_a's stream; consume the initial topology frame.
    let req_a = Request::builder()
        .method("GET")
        .uri(format!(
            "/event-broker/v1/events:stream?subscription_id={id_a}"
        ))
        .body(Body::empty())
        .unwrap();
    let resp_a = harness.router().clone().oneshot(req_a).await.unwrap();
    assert_eq!(resp_a.status(), StatusCode::OK);
    let mut stream_a = resp_a.into_body().into_data_stream();
    tokio::time::timeout(std::time::Duration::from_secs(2), stream_a.next())
        .await
        .expect("initial topology must arrive within 2s")
        .expect("stream must not end")
        .expect("chunk must be ok");

    // sub_b JOINs the same group — coordinator must push a topology frame to sub_a.
    harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "b",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": tenant_id,
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await
        .assert_status(201);

    let rebalance_chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream_a.next())
        .await
        .expect("rebalance topology must arrive within 2s")
        .expect("stream must not end")
        .expect("chunk must be ok");
    let text = String::from_utf8_lossy(&rebalance_chunk);
    assert!(
        text.contains("\"kind\":\"topology\""),
        "expected topology frame, got: {text}"
    );
    assert!(
        text.contains("\"topology_version\":2"),
        "expected version 2, got: {text}"
    );
}

/// sub_a streams, sub_b JOINs and streams, sub_a LEAVEs.
/// The coordinator must push a Terminal control frame into sub_b's open stream.
#[tokio::test]
async fn leave_pushes_terminal_frame_to_surviving_stream() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 4 },
        ])))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tenant_id = Uuid::new_v4();

    let join_sub = |client_agent: &'static str| {
        let harness = &harness;
        let group = group.clone();
        let tenant_id = tenant_id;
        async move {
            harness
                .api_v1()
                .post_subscriptions()
                .with_body(Json(&json!({
                    "consumer_group": group,
                    "client_agent": client_agent,
                    "interests": [{
                        "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                        "tenant_id": tenant_id,
                        "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
                    }],
                })))
                .send()
                .await
                .json()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };

    let id_a = join_sub("a").await;
    let id_b = join_sub("b").await;

    // Seek both — each gets the partitions currently in their DB assignment.
    for (id, parts) in [(&id_a, vec![0u32, 1, 2, 3]), (&id_b, vec![0u32, 1, 2, 3])] {
        harness
            .api_v1()
            .post_subscription_seek(id)
            .with_body(Json(&json!({
                "partition_positions": parts.iter().map(|&p| json!({
                    "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                    "partition": p,
                    "value": "earliest",
                })).collect::<Vec<_>>()
            })))
            .send()
            .await;
    }

    // Open sub_b's stream; drain the initial topology frame.
    let req_b = Request::builder()
        .method("GET")
        .uri(format!(
            "/event-broker/v1/events:stream?subscription_id={id_b}"
        ))
        .body(Body::empty())
        .unwrap();
    let resp_b = harness.router().clone().oneshot(req_b).await.unwrap();
    assert_eq!(resp_b.status(), StatusCode::OK);
    let mut stream_b = resp_b.into_body().into_data_stream();
    tokio::time::timeout(std::time::Duration::from_secs(2), stream_b.next())
        .await
        .expect("initial topology must arrive within 2s")
        .expect("stream must not end")
        .expect("chunk must be ok");

    // sub_a LEAVEs — coordinator must push Terminal to sub_b.
    harness
        .api_v1()
        .delete_subscription(&id_a)
        .send()
        .await
        .assert_status(204);

    let terminal_chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream_b.next())
        .await
        .expect("terminal frame must arrive within 2s")
        .expect("stream must not end")
        .expect("chunk must be ok");
    let text = String::from_utf8_lossy(&terminal_chunk);
    assert!(
        text.contains("\"kind\":\"control\""),
        "expected control frame, got: {text}"
    );
    assert!(
        text.contains("\"code\":\"terminal\""),
        "expected terminal code, got: {text}"
    );
}

// Symmetric to 1.08: multipart/mixed on :sse is rejected in favour of :stream.
#[tokio::test]
async fn sse_events_returns_406_when_accept_is_multipart_mixed() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .get_events_sse("00000000-0000-0000-0000-000000000000")
        .with_header(
            axum::http::header::ACCEPT,
            HeaderValue::from_static("multipart/mixed"),
        )
        .send()
        .await;

    resp.assert_status(406);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 406,
            "detail": "multipart/mixed is served only at /v1/events:stream; this endpoint serves text/event-stream",
            "instance": "/event-broker/v1/events:sse",
            "context": {
                "format": "multipart/mixed is served only at /v1/events:stream; this endpoint serves text/event-stream",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

/// A stream delivers only what its interests match.
///
/// This asserts a *behaviour change*, not a regression guard: the pre-change
/// loop applied no filter at all, so a subscription received every event on its
/// assigned partitions regardless of type - and, since a partition's sequence
/// space is shared, regardless of tenant.
///
/// Both events go to the same partition, so the non-matching one is genuinely
/// read and rejected rather than never seen: the reader's frontier crosses it.
#[tokio::test]
async fn a_stream_delivers_only_events_matching_its_interests() {
    const TOPIC: &str = "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1";
    const WANTED: &str = "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~";
    const UNWANTED: &str = "gts.cf.core.events.event.v1~x.eb.t1.bar.v1~";

    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": TOPIC, "partitions": 1 },
            {
                "id": WANTED,
                "topic": TOPIC,
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
            {
                "id": UNWANTED,
                "topic": TOPIC,
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
        ])))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tenant_id = Uuid::new_v4();
    let id = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "a",
            "interests": [{
                "topic": TOPIC,
                "tenant_id": tenant_id,
                // Only one of the two registered types.
                "types": [WANTED],
            }],
        })))
        .send()
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": TOPIC, "partition": 0, "value": "earliest" }]
        })))
        .send()
        .await
        .assert_status(200);

    // Unwanted first, so a filter that leaked would deliver it first and the
    // assertion below would catch the leak rather than the ordering.
    for (kind, subject) in [(UNWANTED, "s-unwanted"), (WANTED, "s-wanted")] {
        harness
            .api_v1()
            .post_events()
            .with_body(Json(&json!({
                "id": Uuid::new_v4(),
                "type": kind,
                "tenant_id": tenant_id,
                "source": "test-a_stream_delivers_only_events_matching_its_interests",
                "subject": subject,
                "subject_type": "gts.x.eb.t1.subject.v1~",
                "occurred_at": chrono::Utc::now().to_rfc3339(),
            })))
            .send()
            .await
            .assert_status(202);
    }

    let request = Request::builder()
        .method("GET")
        .uri(format!(
            "/event-broker/v1/events:stream?subscription_id={id}"
        ))
        .body(Body::empty())
        .expect("request must build");
    let response = harness
        .router()
        .clone()
        .oneshot(request)
        .await
        .expect("the service itself must not error");
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();

    // Read frames until the wanted event arrives, asserting no `event` frame
    // before it carries the unwanted type.
    let mut delivered_types: Vec<String> = Vec::new();
    for _ in 0..12 {
        let Ok(Some(Ok(chunk))) =
            tokio::time::timeout(std::time::Duration::from_secs(3), stream.next()).await
        else {
            break;
        };
        let text = String::from_utf8_lossy(&chunk).into_owned();
        for part in text.split("\r\n\r\n").skip(1) {
            let body = part.trim_end_matches("\r\n");
            let Ok(frame) = serde_json::from_str::<serde_json::Value>(body) else {
                continue;
            };
            if frame["kind"] == "event" {
                delivered_types.push(frame["payload"]["type"].as_str().unwrap_or("?").to_owned());
            }
        }
        if delivered_types.iter().any(|t| t == WANTED) {
            break;
        }
    }

    assert_eq!(
        delivered_types,
        vec![WANTED.to_owned()],
        "only the matching type may be delivered; the unwanted event shares the \
         partition, so it was read and must have been rejected by the filter"
    );
}
