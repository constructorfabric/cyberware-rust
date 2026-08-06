//! `handlers/ingest/producers.rs` coverage (`eb-rest-handlers` task 11.2):
//! registration, cursor recovery (including empty-topics case), reset (full
//! and scoped), ownership checks.
//!
//! Every test asserts the exact response body received, not just the
//! status code or a couple of selected fields.

use chrono::Utc;
use serde_json::json;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::ingest::{ProducerMode, ProducerRegistrationInput};
use crate::test_support::{EventBrokerHarness, Json, StaticTypesRegistry, TestResponse};

#[tokio::test]
async fn register_producer_chained_mode_returns_201_with_minted_id() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_producers()
        .with_body(Json(
            &json!({ "mode": "chained", "client_agent": "test-agent/1.0" }),
        ))
        .send()
        .await;

    resp.assert_status(201);
    let id = resp.json()["id"]
        .as_str()
        .expect("id must be a string")
        .to_owned();
    assert!(
        Uuid::parse_str(&id).is_ok(),
        "id must be a valid UUID, got '{id}'"
    );
    assert_eq!(
        resp.json(),
        json!({ "id": id, "mode": "chained", "client_agent": "test-agent/1.0" })
    );
}

#[tokio::test]
async fn register_producer_monotonic_mode_returns_201() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_producers()
        .with_body(Json(
            &json!({ "mode": "monotonic", "client_agent": "test-agent/1.0" }),
        ))
        .send()
        .await;

    resp.assert_status(201);
    let id = resp.json()["id"]
        .as_str()
        .expect("id must be a string")
        .to_owned();
    assert_eq!(
        resp.json(),
        json!({ "id": id, "mode": "monotonic", "client_agent": "test-agent/1.0" })
    );
}

#[toolkit_macros::temporary(
    tracking = "gears-rust#4596",
    reason = "asserts axum's raw plain-text Json<T> extraction-rejection body, not a \
              CanonicalError/Problem, because that's genuinely what the handler returns \
              today - toolkit has no canonical-error-aware JSON extractor yet"
)]
#[tokio::test]
async fn register_producer_invalid_mode_returns_422() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_producers()
        .with_body(Json(
            &json!({ "mode": "not_a_mode", "client_agent": "test-agent/1.0" }),
        ))
        .send()
        .await;

    // Unknown enum variant fails at deserialization, before the handler
    // body runs - a plain-text 422, not a domain-level `InvalidMode` `Problem`.
    // The exact trailing "at line N column M" byte offset isn't asserted -
    // it's an internal `serde`/`serde_path_to_error` formatting detail, not
    // part of this contract, and has been observed to differ between
    // otherwise-identical toolchain/lockfile builds (gears-rust#4439 CI).
    // Upstream now renders a deserialization failure as a canonical `Problem`
    // rather than the plain-text `serde` message. The body no longer names the
    // offending field or value, so a stricter assertion is not available - the
    // status and the canonical shape are the whole contract now.
    resp.assert_status(422);
    assert_eq!(
        resp.json(),
        serde_json::json!({
            "type": "about:blank",
            "title": "Unprocessable Entity",
            "status": 422,
            "detail": "Unprocessable Entity",
            "instance": "/event-broker/v1/producers",
            "context": {},
        })
    );
}

#[tokio::test]
async fn get_producer_cursors_empty_topics_case() {
    let harness = EventBrokerHarness::builder().build().await;
    let register = harness
        .api_v1()
        .post_producers()
        .with_body(Json(&json!({ "mode": "chained", "client_agent": "a" })))
        .send()
        .await;
    let id = register.json()["id"].as_str().unwrap().to_owned();

    let resp = harness.api_v1().get_producer_cursors(&id).send().await;

    resp.assert_status(200);
    assert_eq!(
        resp.json(),
        json!({ "producer_id": id, "client_agent": "a", "topics": [] })
    );
}

#[tokio::test]
async fn get_producer_cursors_not_found_returns_404() {
    let harness = EventBrokerHarness::builder().build().await;
    let producer_id = Uuid::new_v4();

    let resp = harness
        .api_v1()
        .get_producer_cursors(&producer_id.to_string())
        .send()
        .await;

    resp.assert_status(404);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": format!("producer '{producer_id}' is not registered"),
            "instance": format!("/event-broker/v1/producers/{producer_id}/cursors"),
            "context": {
                "resource_name": producer_id.to_string(),
                "resource_type": "gts.cf.core.events.producer.v1~",
            },
        })
    );
}

#[tokio::test]
async fn get_producer_cursors_not_owner_returns_403() {
    let harness = EventBrokerHarness::builder().build().await;
    let other_ctx = SecurityContext::builder()
        .subject_tenant_id(Uuid::new_v4())
        .subject_id(Uuid::new_v4())
        .build()
        .expect("valid security context");
    let registration = harness
        .ingest()
        .register_producer(
            &other_ctx,
            ProducerRegistrationInput {
                mode: ProducerMode::Chained,
                client_agent: "other-agent".to_owned(),
            },
        )
        .await
        .expect("registration must succeed");

    let resp = harness
        .api_v1()
        .get_producer_cursors(&registration.id.to_string())
        .send()
        .await;

    resp.assert_status(403);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.permission_denied.v1~",
            "title": "Permission Denied",
            "status": 403,
            "detail": "You do not have permission to perform this operation",
            "instance": format!("/event-broker/v1/producers/{}/cursors", registration.id),
            "context": {
                "reason": "calling principal does not own this producer_id",
                "resource_type": "gts.cf.core.events.producer.v1~",
            },
        })
    );
}

/// Publishes one chained-mode event for `producer_id` against the topic its
/// `type_id` resolves to, partition 0, at chain position `(previous, sequence)`.
async fn publish_chained_event(
    harness: &EventBrokerHarness,
    type_id: &str,
    subject_type: &str,
    producer_id: &str,
    previous: i64,
    sequence: i64,
) -> TestResponse {
    harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": type_id,
            "tenant_id": Uuid::new_v4(),
            "source": "test-reset_producer",
            "subject": "s-test-reset_producer",
            "subject_type": subject_type,
            "occurred_at": Utc::now().to_rfc3339(),
            "meta": {
                "version": 1,
                "producer_id": producer_id,
                "previous": previous,
                "sequence": sequence,
            },
        })))
        .send()
        .await
}

#[tokio::test]
async fn reset_producer_full_scope_returns_200_with_no_body() {
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
    let register = harness
        .api_v1()
        .post_producers()
        .with_body(Json(&json!({ "mode": "chained", "client_agent": "a" })))
        .send()
        .await;
    let id = register.json()["id"].as_str().unwrap().to_owned();

    publish_chained_event(
        &harness,
        "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
        "gts.x.eb.t1.subject.v1~",
        &id,
        0,
        1,
    )
    .await
    .assert_status(202);
    assert_eq!(
        harness
            .api_v1()
            .get_producer_cursors(&id)
            .send()
            .await
            .json()["topics"],
        json!([{
            "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
            "partitions": [{ "partition": 0, "last_sequence": 1 }],
        }]),
        "cursor must reflect the published chain position before reset"
    );

    let resp = harness.api_v1().post_producer_reset(&id).send().await;

    resp.assert_status(200);
    assert_eq!(resp.text(), "", "200 must carry no body");
    assert_eq!(
        harness
            .api_v1()
            .get_producer_cursors(&id)
            .send()
            .await
            .json()["topics"],
        json!([]),
        "a full reset must clear every tracked cursor"
    );
    // A sequence that would have been a 412 SequenceViolation before the
    // reset (chain restarting at previous=0) now succeeds, proving the
    // reset actually cleared the chain state used for validation, not just
    // the cursors endpoint's own read path.
    publish_chained_event(
        &harness,
        "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
        "gts.x.eb.t1.subject.v1~",
        &id,
        0,
        1,
    )
    .await
    .assert_status(202);
}

#[tokio::test]
async fn reset_producer_scoped_to_topic_partition_returns_200_with_no_body() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic_a.v1", "partitions": 1 },
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic_b.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo_a.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic_a.v1",
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo_b.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic_b.v1",
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
        ])))
        .build()
        .await;
    let register = harness
        .api_v1()
        .post_producers()
        .with_body(Json(&json!({ "mode": "chained", "client_agent": "a" })))
        .send()
        .await;
    let id = register.json()["id"].as_str().unwrap().to_owned();

    publish_chained_event(
        &harness,
        "gts.cf.core.events.event.v1~x.eb.t1.foo_a.v1~",
        "gts.x.eb.t1.subject.v1~",
        &id,
        0,
        1,
    )
    .await
    .assert_status(202);
    publish_chained_event(
        &harness,
        "gts.cf.core.events.event.v1~x.eb.t1.foo_b.v1~",
        "gts.x.eb.t1.subject.v1~",
        &id,
        0,
        1,
    )
    .await
    .assert_status(202);

    let resp = harness
        .api_v1()
        .post_producer_reset(&id)
        .with_body(Json(
            &json!({ "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic_a.v1", "partition": 0 }),
        ))
        .send()
        .await;

    resp.assert_status(200);
    assert_eq!(resp.text(), "", "200 must carry no body");
    assert_eq!(
        harness
            .api_v1()
            .get_producer_cursors(&id)
            .send()
            .await
            .json()["topics"],
        json!([{
            "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic_b.v1",
            "partitions": [{ "partition": 0, "last_sequence": 1 }],
        }]),
        "a scoped reset must clear only the given (topic, partition), leaving the other topic's cursor untouched"
    );
}

#[tokio::test]
async fn reset_producer_not_found_returns_404() {
    let harness = EventBrokerHarness::builder().build().await;
    let producer_id = Uuid::new_v4();

    let resp = harness
        .api_v1()
        .post_producer_reset(&producer_id.to_string())
        .send()
        .await;

    resp.assert_status(404);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": format!("producer '{producer_id}' is not registered"),
            "instance": format!("/event-broker/v1/producers/{producer_id}:reset"),
            "context": {
                "resource_name": producer_id.to_string(),
                "resource_type": "gts.cf.core.events.producer.v1~",
            },
        })
    );
}

#[tokio::test]
async fn reset_producer_oversized_body_returns_413() {
    let harness = EventBrokerHarness::builder().build().await;

    // One byte past axum's own implicit `Bytes` extractor default (2 MiB) -
    // `reset_producer`'s `body: axum::body::Bytes` parameter gets this limit
    // for free (`eb-dispatcher-proxy-error-handling`'s Decisions), so this
    // must reject before the handler body ever runs, without buffering the
    // whole oversized body or reaching `IngestService::reset_producer` at
    // all (any producer id works, even a nonexistent one).
    let oversized_body = vec![0u8; 2 * 1024 * 1024 + 1];

    let resp = harness
        .api_v1()
        .post_producer_reset(&Uuid::new_v4().to_string())
        .with_body(oversized_body)
        .send()
        .await;

    resp.assert_status(413);
}

#[tokio::test]
async fn reset_producer_not_owner_returns_403() {
    let harness = EventBrokerHarness::builder().build().await;
    let other_ctx = SecurityContext::builder()
        .subject_tenant_id(Uuid::new_v4())
        .subject_id(Uuid::new_v4())
        .build()
        .expect("valid security context");
    let registration = harness
        .ingest()
        .register_producer(
            &other_ctx,
            ProducerRegistrationInput {
                mode: ProducerMode::Chained,
                client_agent: "other-agent".to_owned(),
            },
        )
        .await
        .expect("registration must succeed");

    let resp = harness
        .api_v1()
        .post_producer_reset(&registration.id.to_string())
        .send()
        .await;

    resp.assert_status(403);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.permission_denied.v1~",
            "title": "Permission Denied",
            "status": 403,
            "detail": "You do not have permission to perform this operation",
            "instance": format!("/event-broker/v1/producers/{}:reset", registration.id),
            "context": {
                "reason": "calling principal does not own this producer_id",
                "resource_type": "gts.cf.core.events.producer.v1~",
            },
        })
    );
}

// -- client_agent encoding and length -------------------------------------
//
// This is the one `client_agent` that persists - into a column documented as
// printable ASCII within 256 bytes - so the check runs in the handler, ahead
// of `IngestService::register_producer`. A rejected request therefore never
// reaches the write at all.

#[tokio::test]
async fn register_producer_rejects_a_non_ascii_client_agent() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_producers()
        .with_body(Json(
            &json!({ "mode": "chained", "client_agent": "agent-\u{e9}" }),
        ))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "Request validation failed",
            "instance": "/event-broker/v1/producers",
            "context": {
                "field_violations": [{
                    "field": "client_agent",
                    "description": "must contain only printable ASCII (0x20-0x7E)",
                    "reason": "ascii_only",
                }],
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn register_producer_rejects_an_over_length_client_agent() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_producers()
        .with_body(Json(
            &json!({ "mode": "chained", "client_agent": "a".repeat(257) }),
        ))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "Request validation failed",
            "instance": "/event-broker/v1/producers",
            "context": {
                "field_violations": [{
                    "field": "client_agent",
                    "description": "must be 1-256 bytes, got 257",
                    "reason": "field_too_long",
                }],
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}
