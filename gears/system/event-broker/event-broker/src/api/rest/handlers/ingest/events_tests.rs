//! `handlers/ingest/events.rs` coverage (`eb-rest-handlers` task 11.1): happy path
//! and reachable Hard-Error Catalog codes for `POST /v1/events`/`:batch`,
//! driven through `EventBrokerHarness::api_v1()`.
//!
//! Every test inlines the exact request body sent and asserts the exact
//! response body received (not just the status code) - no shared
//! request/fixture builders, so nothing about what's sent or checked is
//! hidden outside the test itself. Topic/event-type fixtures go through
//! `StaticTypesRegistry::of(...)` (`test_support::type_registry`), which
//! validates every id as a real GTS instance id.

use std::sync::Arc;

use authz_resolver_sdk::{EvaluationRequest, PolicyEnforcer};
use chrono::Utc;
use serde_json::json;
use toolkit_security::pep_properties;
use uuid::Uuid;

use crate::domain::ingest::{ProducerMode, ProducerRegistrationInput, PublishRequest};
use crate::test_support::{DenyingAuthZ, EventBrokerHarness, Json, StaticTypesRegistry};

/// Registers a producer via the real `IngestService::register_producer`
/// (not a bare `Uuid::new_v4()`) - `Storage`'s `check_and_enqueue` looks up
/// the producer row before accepting its first chained event
/// (eb-single-process-implementation D2 risk mitigation swapped the
/// permissive `InMemoryDomainRepo` stand-in, which never checked this, for
/// the real `Storage`), so a producer chain test needs a producer that
/// genuinely exists.
async fn register_test_producer(harness: &EventBrokerHarness) -> Uuid {
    harness
        .ingest()
        .register_producer(
            harness.security_context(),
            ProducerRegistrationInput {
                mode: ProducerMode::Chained,
                client_agent: "test-agent".to_owned(),
            },
        )
        .await
        .expect("producer registration must succeed")
        .id
}

#[tokio::test]
async fn publish_event_happy_path_returns_202_accepted() {
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

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~", // [todo]: I don't think we are allowed to use gts.cf without `gts!` macros
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_happy_path_returns_202_accepted",
            "subject": "s-publish_event_happy_path_returns_202_accepted",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(202);
    assert_eq!(resp.text(), "", "202 Accepted must carry no body");
}

#[tokio::test]
async fn publish_event_sync_wait_header_returns_201_created() {
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

    let resp = harness
        .api_v1()
        .post_events()
        .with_header(
            axum::http::header::HeaderName::from_static("sync-wait"),
            axum::http::HeaderValue::from_static("true"),
        )
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_sync_wait_header_returns_201_created",
            "subject": "s-publish_event_sync_wait_header_returns_201_created",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(201);
    assert_eq!(resp.text(), "", "201 Created must carry no body");
}

#[tokio::test]
async fn publish_event_sync_wait_query_returns_201_created() {
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

    let resp = harness
        .api_v1()
        .post_events()
        .with_query("wait", "persisted")
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_sync_wait_query_returns_201_created",
            "subject": "s-publish_event_sync_wait_query_returns_201_created",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(201);
    assert_eq!(resp.text(), "", "201 Created must carry no body");
}

/// A registered event type whose `topic` names a stream nobody registered is
/// the only route left to `TopicNotFound` on publish: an event names no topic
/// of its own, so the producer cannot ask for a missing one.
#[tokio::test]
async fn publish_event_whose_type_names_an_unregistered_topic_returns_404_topic_not_found() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.missing.topic.v1",
                "topic_registered": false,
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_whose_type_names_an_unregistered_topic",
            "subject": "s-publish_event_whose_type_names_an_unregistered_topic",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(404);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": "topic 'gts.cf.core.events.topic.v1~x.eb.missing.topic.v1' is not registered",
            "instance": "/event-broker/v1/events",
            "context": {
                "resource_name": "gts.cf.core.events.topic.v1~x.eb.missing.topic.v1",
                "resource_type": "gts.cf.core.events.topic.v1~",
            },
        })
    );
}

#[toolkit_macros::temporary(
    tracking = "gears-rust#4596",
    reason = "asserts axum's raw plain-text Json<T> extraction-rejection body, not a \
              CanonicalError/Problem, because that's genuinely what the handler returns \
              today - toolkit has no canonical-error-aware JSON extractor yet"
)]
#[tokio::test]
async fn publish_event_readonly_field_rejected_via_deny_unknown_fields() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;

    // `sequence` is one of the schema's `readOnly` fields - a client that
    // supplies it is rejected by `#[serde(deny_unknown_fields)]` before the
    // handler body ever runs, at deserialization time (a plain-text 422,
    // not a `Problem` JSON body). `id`/`tenant_id`/`occurred_at` are fixed
    // literals, not `Uuid::new_v4()`/`Utc::now()` - the assertion below
    // pins the exact response body byte-for-byte (including the trailing
    // "at line N column M" `serde_path_to_error` offset), which requires a
    // byte-stable request body; `Utc::now().to_rfc3339()`'s fractional-second
    // digit count varies run to run and previously made this offset flaky
    // (gears-rust#4439 CI).
    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": "5c1f7b2e-2a34-4e3a-8a1a-8f6e6f2b9c11",
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": "9d3e4f5a-6b7c-4d8e-9f0a-1b2c3d4e5f6a",
            "source": "test-publish_event_readonly_field_rejected_via_deny_unknown_fields",
            "subject": "s-publish_event_readonly_field_rejected_via_deny_unknown_fields",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": "2026-01-01T00:00:00Z",
            "sequence": 1,
        })))
        .send()
        .await;

    // Upstream now renders a deserialization failure as a canonical `Problem`
    // rather than the plain-text `serde` message, so the body no longer names
    // the rejected field. What this still pins is the important half: a
    // read-only field on publish input is refused rather than ignored.
    resp.assert_status(422);
    assert_eq!(
        resp.json(),
        serde_json::json!({
            "type": "about:blank",
            "title": "Unprocessable Entity",
            "status": 422,
            "detail": "Unprocessable Entity",
            "instance": "/event-broker/v1/events",
            "context": {},
        })
    );
}

#[tokio::test]
async fn publish_event_schema_violation_returns_400() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "data_schema": {
                    "type": "object",
                    "required": ["required_field"],
                    "additionalProperties": false,
                },
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_schema_violation_returns_400",
            "subject": "s-publish_event_schema_violation_returns_400",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
            "data": {},
        })))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "SchemaViolation: \"required_field\" is a required property",
            "instance": "/event-broker/v1/events",
            "context": {
                "format": "SchemaViolation: \"required_field\" is a required property",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn publish_event_sequence_violation_returns_412_with_expected_previous() {
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
    let producer_id = register_test_producer(&harness).await;

    harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_sequence_violation_returns_412_with_expected_previous",
            "subject": "s-publish_event_sequence_violation_returns_412_with_expected_previous",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
            "meta": { "version": 1, "producer_id": producer_id, "previous": 0, "sequence": 1 },
        })))
        .send()
        .await
        .assert_status(202);

    // Skips straight to sequence 3 instead of 2 - a genuine chain break.
    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_sequence_violation_returns_412_with_expected_previous",
            "subject": "s-publish_event_sequence_violation_returns_412_with_expected_previous",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
            "meta": { "version": 1, "producer_id": producer_id, "previous": 1, "sequence": 3 },
        })))
        .send()
        .await;

    resp.assert_status(412);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
            "title": "Failed Precondition",
            "status": 412,
            "detail": "Operation precondition not met",
            "instance": "/event-broker/v1/events",
            "context": {
                "resource_type": "gts.cf.core.events.request.v1~",
                "violations": [{
                    "type": "sequence_mismatch",
                    "subject": "(producer)",
                    "description": "topic=gts.cf.core.events.topic.v1~x.eb.t1.topic.v1 \
                                     partition=0 expected_previous=1",
                }],
            },
        })
    );
}

#[tokio::test]
async fn publish_event_duplicate_resubmission_is_ignored_not_an_error() {
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
    let producer_id = register_test_producer(&harness).await;
    let body = json!({
        "id": Uuid::new_v4(),
        "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
        "tenant_id": Uuid::new_v4(),
        "source": "test-publish_event_duplicate_resubmission_is_ignored_not_an_error",
        "subject": "s-publish_event_duplicate_resubmission_is_ignored_not_an_error",
        "subject_type": "gts.x.eb.t1.subject.v1~",
        "occurred_at": Utc::now().to_rfc3339(),
        "meta": { "version": 1, "producer_id": producer_id, "previous": 0, "sequence": 1 },
    });

    harness
        .api_v1()
        .post_events()
        .with_body(Json(&body))
        .send()
        .await
        .assert_status(202);

    // Same sequence resubmitted - accepted (ignored), not a sequence violation.
    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&body))
        .send()
        .await;
    resp.assert_status(202);
    assert_eq!(resp.text(), "", "202 Accepted must carry no body");
}

#[tokio::test]
async fn publish_batch_happy_path_returns_202() {
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

    let resp = harness
        .api_v1()
        .post_events_batch()
        .with_body(Json(&json!({
            "events": [
                {
                    "id": Uuid::new_v4(),
                    "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                    "tenant_id": Uuid::new_v4(),
                    "source": "test-publish_batch_happy_path_returns_202",
                    "subject": "s-publish_batch_happy_path_returns_202",
                    "subject_type": "gts.x.eb.t1.subject.v1~",
                    "occurred_at": Utc::now().to_rfc3339(),
                },
                {
                    "id": Uuid::new_v4(),
                    "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                    "tenant_id": Uuid::new_v4(),
                    "source": "test-publish_batch_happy_path_returns_202",
                    "subject": "s-publish_batch_happy_path_returns_202",
                    "subject_type": "gts.x.eb.t1.subject.v1~",
                    "occurred_at": Utc::now().to_rfc3339(),
                },
            ],
        })))
        .send()
        .await;

    resp.assert_status(202);
    assert_eq!(resp.text(), "", "202 Accepted must carry no body");
}

#[tokio::test]
/// Two event types on two different topics: nothing in the request bodies says
/// so, and the rejection comes from resolving each `type` to the topic its
/// `topic` trait names.
async fn publish_batch_mixed_topics_returns_400() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            { "id": "gts.cf.core.events.topic.v1~x.eb.t2.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t2.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t2.topic.v1",
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events_batch()
        .with_body(Json(&json!({
            "events": [
                {
                    "id": Uuid::new_v4(),
                    "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                    "tenant_id": Uuid::new_v4(),
                    "source": "test-publish_batch_mixed_topics_returns_400",
                    "subject": "s-publish_batch_mixed_topics_returns_400",
                    "subject_type": "gts.x.eb.t1.subject.v1~",
                    "occurred_at": Utc::now().to_rfc3339(),
                },
                {
                    "id": Uuid::new_v4(),
                    "type": "gts.cf.core.events.event.v1~x.eb.t2.foo.v1~",
                    "tenant_id": Uuid::new_v4(),
                    "source": "test-publish_batch_mixed_topics_returns_400",
                    "subject": "s-publish_batch_mixed_topics_returns_400",
                    "subject_type": "gts.x.eb.t1.subject.v1~",
                    "occurred_at": Utc::now().to_rfc3339(),
                },
            ],
        })))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "MixedTopics: a batch must target a single topic",
            "instance": "/event-broker/v1/events:batch",
            "context": {
                "format": "MixedTopics: a batch must target a single topic",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn publish_batch_too_large_returns_413() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;

    let events: Vec<_> = (0..101)
        .map(|_| {
            json!({
                "id": Uuid::new_v4(),
                "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "tenant_id": Uuid::new_v4(),
                "source": "test-publish_batch_too_large_returns_413",
                "subject": "s-publish_batch_too_large_returns_413",
                "subject_type": "gts.x.eb.t1.subject.v1~",
                "occurred_at": Utc::now().to_rfc3339(),
            })
        })
        .collect();
    let resp = harness
        .api_v1()
        .post_events_batch()
        .with_body(Json(&json!({ "events": events })))
        .send()
        .await;

    resp.assert_status(413);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 413,
            "detail": "batch too large: 101 events (max 100)",
            "instance": "/event-broker/v1/events:batch",
            "context": {
                "format": "batch too large: 101 events (max 100)",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn publish_batch_mid_batch_sequence_violation_aborts_the_batch() {
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
    let producer_id = register_test_producer(&harness).await;

    let resp = harness
        .api_v1()
        .post_events_batch()
        .with_body(Json(&json!({
            "events": [
                {
                    "id": Uuid::new_v4(),
                    "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                    "tenant_id": Uuid::new_v4(),
                    "source": "test-publish_batch_mid_batch_sequence_violation_aborts_the_batch",
                    "subject": "s-publish_batch_mid_batch_sequence_violation_aborts_the_batch",
                    "subject_type": "gts.x.eb.t1.subject.v1~",
                    "occurred_at": Utc::now().to_rfc3339(),
                    "meta": { "version": 1, "producer_id": producer_id, "previous": 0, "sequence": 1 },
                },
                {
                    "id": Uuid::new_v4(),
                    "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                    "tenant_id": Uuid::new_v4(),
                    "source": "test-publish_batch_mid_batch_sequence_violation_aborts_the_batch",
                    "subject": "s-publish_batch_mid_batch_sequence_violation_aborts_the_batch",
                    "subject_type": "gts.x.eb.t1.subject.v1~",
                    "occurred_at": Utc::now().to_rfc3339(),
                    // Skips straight to sequence 5 - a genuine chain break.
                    "meta": { "version": 1, "producer_id": producer_id, "previous": 1, "sequence": 5 },
                },
            ],
        })))
        .send()
        .await;

    resp.assert_status(412);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.failed_precondition.v1~",
            "title": "Failed Precondition",
            "status": 412,
            "detail": "Operation precondition not met",
            "instance": "/event-broker/v1/events:batch",
            "context": {
                "resource_type": "gts.cf.core.events.request.v1~",
                "violations": [{
                    "type": "sequence_mismatch",
                    "subject": "(producer)",
                    "description": "topic=gts.cf.core.events.topic.v1~x.eb.t1.topic.v1 \
                                     partition=0 expected_previous=1",
                }],
            },
        })
    );
}

// -- authz enforcement (`gears-rust#4516`, `eb-authz-enforcement`) --

#[tokio::test]
async fn publish_rejected_for_missing_event_type_produce_permission_returns_403_before_type_lookup()
{
    let harness = EventBrokerHarness::builder()
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: |req: &EvaluationRequest| req.action.name == "produce",
        })))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            // Never seeded - proves the check runs before the event-type
            // lookup that resolves the topic (a 404 here instead of 403 would
            // mean the ordering regressed).
            "type": "gts.cf.core.events.event.v1~x.eb.missing.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_rejected_for_missing_event_type_produce_permission_returns_403_before_type_lookup",
            "subject": "s-publish_rejected_for_missing_event_type_produce_permission_returns_403_before_type_lookup",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
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
            "instance": "/event-broker/v1/events",
            "context": {
                "reason": "access denied by policy",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn publish_rejected_for_unauthorized_tenant_id_returns_403() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            // The tenant-scope check is its own `PolicyEnforcer` call
            // (`domain::authz::TENANT_SCOPE_RESOURCE`), distinguishable by
            // the `OWNER_TENANT_ID` resource property it alone sets.
            deny_if: |req: &EvaluationRequest| {
                req.resource
                    .properties
                    .contains_key(pep_properties::OWNER_TENANT_ID)
            },
        })))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_rejected_for_unauthorized_tenant_id_returns_403",
            "subject": "s-publish_rejected_for_unauthorized_tenant_id_returns_403",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
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
            "instance": "/event-broker/v1/events",
            "context": {
                "reason": "access denied by policy",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn publish_succeeds_when_produce_permission_and_tenant_checks_both_pass() {
    let allowed_tenant_id = Uuid::new_v4();
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
        ])))
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            // Allows only the exact event type and tenant this test
            // publishes as - proves the real values get checked, not a
            // hardcoded pass.
            deny_if: move |req: &EvaluationRequest| {
                let allowed_event_type = req.resource.properties.get("event_type_id")
                    == Some(&json!("gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"));
                let allowed_tenant = req.resource.properties.get(pep_properties::OWNER_TENANT_ID)
                    == Some(&json!(allowed_tenant_id));
                !(allowed_event_type || allowed_tenant)
            },
        })))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": allowed_tenant_id,
            "source": "test-publish_succeeds_when_produce_permission_and_tenant_checks_both_pass",
            "subject": "s-publish_succeeds_when_produce_permission_and_tenant_checks_both_pass",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(202);
    assert_eq!(resp.text(), "", "202 Accepted must carry no body");
}

#[tokio::test]
async fn publish_batch_rejects_the_first_unauthorized_event_but_still_accepts_the_rest() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
            // On the same topic as `foo.v1`, so the batch is homogeneous and
            // the denial below is the only thing that rejects an event.
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.bar.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
        ])))
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: |req: &EvaluationRequest| {
                req.resource.properties.get("event_type_id")
                    == Some(&json!("gts.cf.core.events.event.v1~x.eb.t1.bar.v1~"))
            },
        })))
        .build()
        .await;
    let allowed_id = Uuid::new_v4();
    let denied_id = Uuid::new_v4();
    let requests = vec![
        PublishRequest {
            id: allowed_id,
            r#type: crate::test_support::event_type_id(
                "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            ),
            tenant_id: Uuid::new_v4(),
            source: "test".to_owned(),
            subject: "s1".to_owned(),
            subject_type: "gts.x.eb.t1.subject.v1~".to_owned(),
            occurred_at: Utc::now(),
            trace_parent: None,
            data: json!({}),
            meta: None,
        },
        PublishRequest {
            id: denied_id,
            r#type: crate::test_support::event_type_id(
                "gts.cf.core.events.event.v1~x.eb.t1.bar.v1~",
            ),
            tenant_id: Uuid::new_v4(),
            source: "test".to_owned(),
            subject: "s1".to_owned(),
            subject_type: "gts.x.eb.t1.subject.v1~".to_owned(),
            occurred_at: Utc::now(),
            trace_parent: None,
            data: json!({}),
            meta: None,
        },
    ];

    let result = harness
        .ingest()
        .publish_batch(harness.security_context(), requests)
        .await
        .expect(
            "publish_batch itself must not error - per-event denial is recorded in \
             BatchResult::failed, not surfaced as a batch-level error",
        );

    assert_eq!(result.accepted, vec![allowed_id]);
    assert_eq!(result.failed.len(), 1);
    assert_eq!(result.failed[0].0, denied_id);
    assert_eq!(
        result.failed[0].1,
        "NotAuthorizedToProduce: access denied by policy"
    );
}

// -- event-type / subject-type enforcement (`eb-event-type-enforcement`) --

#[tokio::test]
async fn publish_event_unregistered_event_type_returns_404_event_type_not_found() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.unregistered.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_unregistered_event_type_returns_404_event_type_not_found",
            "subject": "s-publish_event_unregistered_event_type_returns_404_event_type_not_found",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(404);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": "event type 'gts.cf.core.events.event.v1~x.eb.t1.unregistered.v1~' is not registered",
            "instance": "/event-broker/v1/events",
            "context": {
                "resource_name": "gts.cf.core.events.event.v1~x.eb.t1.unregistered.v1~",
                "resource_type": "gts.cf.core.events.event_type.v1~",
            },
        })
    );
}

#[tokio::test]
async fn publish_event_with_nothing_registered_returns_404_event_type_not_found() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.unregistered.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_with_nothing_registered",
            "subject": "s-publish_event_with_nothing_registered",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    // The event type resolves the topic, so with nothing registered the type
    // lookup is the one that fails - `EventTypeNotFound`, never
    // `TopicNotFound`, which no longer has an input of its own to miss on.
    resp.assert_status(404);
    assert_eq!(
        resp.json()["context"]["resource_type"],
        json!("gts.cf.core.events.event_type.v1~")
    );
}

#[tokio::test]
async fn publish_event_malformed_subject_type_returns_400() {
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

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_malformed_subject_type_returns_400",
            "subject": "s-publish_event_malformed_subject_type_returns_400",
            "subject_type": "not-a-gts-id",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(400);
    let body = resp.json();
    assert_eq!(
        body["type"],
        json!("gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~")
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .starts_with("InvalidSubjectType:"),
        "detail must start with 'InvalidSubjectType:', got: {}",
        body["detail"]
    );
}

/// One producer cannot choose grouping per message: the field is gone from the
/// schema, and a body that carries it is refused rather than accepted and
/// ignored - which would leave the publisher believing it had chosen.
///
/// The refusal happens at deserialization, so it is the platform's canonical
/// unprocessable-entity envelope rather than a domain `Problem` naming the
/// field - the same shape a read-only field on publish input produces (see
/// `publish_event_readonly_field_rejected_via_deny_unknown_fields`). What this
/// pins is the half that matters: refused, not ignored. The literals are fixed
/// rather than generated so the body is byte-stable.
#[tokio::test]
async fn publish_event_carrying_a_partition_key_is_refused() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 4 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": "5c1f7b2e-2a34-4e3a-8a1a-8f6e6f2b9c11",
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": "9d3e4f5a-6b7c-4d8e-9f0a-1b2c3d4e5f6a",
            "source": "test-publish_event_carrying_a_partition_key_is_refused",
            "subject": "s-publish_event_carrying_a_partition_key_is_refused",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": "2026-01-01T00:00:00Z",
            "partition_key": "chosen-by-the-publisher",
        })))
        .send()
        .await;

    resp.assert_status(422);
    assert_eq!(
        resp.json(),
        json!({
            "type": "about:blank",
            "title": "Unprocessable Entity",
            "status": 422,
            "detail": "Unprocessable Entity",
            "instance": "/event-broker/v1/events",
            "context": {},
        })
    );
}

/// The event's own type is a GTS **type** identifier: a concrete event type is
/// a derived type schema, so an identifier without the trailing `~` names
/// nothing that can exist. Rejected in the DTO, on the same terms as any other
/// malformed identifier, rather than reaching a lookup that would answer
/// "unregistered" and send the publisher looking for a registration problem.
#[tokio::test]
async fn publish_event_instance_shaped_type_returns_400() {
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

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            // The same registered type, spelled as an instance would be.
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_instance_shaped_type_returns_400",
            "subject": "s-publish_event_instance_shaped_type_returns_400",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(400);
    let body = resp.json();
    assert!(
        body["detail"].as_str().unwrap().starts_with("InvalidBody:"),
        "detail must start with 'InvalidBody:', got: {}",
        body["detail"]
    );
}

/// The other side of the same contract: the identifier every committed scenario
/// writes - a derived event type, trailing `~` and all - is accepted.
#[tokio::test]
async fn publish_event_with_a_scenario_shaped_type_returns_202() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~acme.shop._.orders.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~acme.shop.orders.created.v1~",
                "topic": "gts.cf.core.events.topic.v1~acme.shop._.orders.v1",
                "allowed_subject_types": ["gts.acme.shop.orders.order.v1~"],
            },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~acme.shop.orders.created.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_with_a_scenario_shaped_type_returns_202",
            "subject": "order-1",
            "subject_type": "gts.acme.shop.orders.order.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(202);
    assert_eq!(resp.text(), "", "202 Accepted carries no body");
}

#[tokio::test]
async fn publish_event_instance_shaped_subject_type_returns_400() {
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

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_instance_shaped_subject_type_returns_400",
            "subject": "s-publish_event_instance_shaped_subject_type_returns_400",
            // Well-formed GTS id, but an Instance (no trailing '~') - proves
            // the type-vs-instance check, not just a raw parse failure.
            "subject_type": "gts.x.eb.t1.subject.v1~a.b.c.d.v1",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(400);
    let body = resp.json();
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .starts_with("InvalidSubjectType:"),
        "detail must start with 'InvalidSubjectType:', got: {}",
        body["detail"]
    );
}

#[tokio::test]
async fn publish_event_subject_type_exact_match_returns_202() {
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

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_subject_type_exact_match_returns_202",
            "subject": "s-publish_event_subject_type_exact_match_returns_202",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(202);
}

#[tokio::test]
async fn publish_event_subject_type_wildcard_suffix_match_returns_202() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~acme.*"],
            },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_subject_type_wildcard_suffix_match_returns_202",
            "subject": "s-publish_event_subject_type_wildcard_suffix_match_returns_202",
            // Chained under the wildcard's "acme" prefix - a Type, still
            // ending in '~'.
            "subject_type": "gts.x.eb.t1.subject.v1~acme.corp.internal.vip_subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(202);
}

#[tokio::test]
async fn publish_event_subject_type_wildcard_wrong_prefix_returns_400() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~acme.*"],
            },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_subject_type_wildcard_wrong_prefix_returns_400",
            "subject": "s-publish_event_subject_type_wildcard_wrong_prefix_returns_400",
            // Same shape as the wildcard-match test, but chained under a
            // different vendor ("other", not "acme") - proves the
            // wildcard's fixed prefix is actually enforced, not a
            // match-everything fallback (`GlobalTypeSystem/gts-spec` §3.5).
            "subject_type": "gts.x.eb.t1.subject.v1~other.corp.internal.vip_subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(400);
    let body = resp.json();
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .starts_with("SubjectTypeNotAllowed:"),
        "detail must start with 'SubjectTypeNotAllowed:', got: {}",
        body["detail"]
    );
}

#[tokio::test]
async fn publish_event_subject_type_bare_base_implicit_coverage_returns_202() {
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

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_subject_type_bare_base_implicit_coverage_returns_202",
            "subject": "s-publish_event_subject_type_bare_base_implicit_coverage_returns_202",
            // Chained under the bare-base pattern - matched via GTS's
            // "implicit derived-type coverage" (`gts-spec` §3.6), not exact
            // string equality.
            "subject_type": "gts.x.eb.t1.subject.v1~acme.corp.internal.vip_subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(202);
}

#[tokio::test]
async fn publish_event_subject_type_not_allowed_returns_400() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "allowed_subject_types": ["gts.x.eb.t1.other_subject.v1~"],
            },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_subject_type_not_allowed_returns_400",
            "subject": "s-publish_event_subject_type_not_allowed_returns_400",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "SubjectTypeNotAllowed: subject_type 'gts.x.eb.t1.subject.v1~' is not in \
                       event type 'gts.cf.core.events.event.v1~x.eb.t1.foo.v1~''s \
                       allowed_subject_types",
            "instance": "/event-broker/v1/events",
            "context": {
                "format": "SubjectTypeNotAllowed: subject_type 'gts.x.eb.t1.subject.v1~' is not \
                           in event type 'gts.cf.core.events.event.v1~x.eb.t1.foo.v1~''s \
                           allowed_subject_types",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn publish_event_empty_allowed_subject_types_rejects_everything() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                // No `allowed_subject_types` key at all - defaults to `[]`.
            },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_empty_allowed_subject_types_rejects_everything",
            "subject": "s-publish_event_empty_allowed_subject_types_rejects_everything",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(400);
    let body = resp.json();
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .starts_with("SubjectTypeNotAllowed:"),
        "detail must start with 'SubjectTypeNotAllowed:', got: {}",
        body["detail"]
    );
}

#[tokio::test]
async fn publish_event_unregistered_event_type_short_circuits_subject_type_check() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.unregistered.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "test-publish_event_unregistered_event_type_short_circuits_subject_type_check",
            "subject": "s-publish_event_unregistered_event_type_short_circuits_subject_type_check",
            // Malformed - would also fail the subject-type check if it were
            // ever reached. It must not be: event-type lookup (step 4) runs
            // before subject-type membership (step 5).
            "subject_type": "not-a-gts-id",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(404);
    let body = resp.json();
    assert_eq!(
        body["context"]["resource_type"],
        json!("gts.cf.core.events.event_type.v1~")
    );
}

// -- envelope field encoding and length -----------------------------------
//
// The validation pipeline puts schema-level field checks ahead of event-type
// lookup, so these run before the type is resolved.

fn envelope_violation(field: &str, description: &str, reason: &str) -> serde_json::Value {
    json!({
        "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
        "title": "Invalid Argument",
        "status": 400,
        "detail": "Request validation failed",
        "instance": "/event-broker/v1/events",
        "context": {
            "field_violations": [{
                "field": field,
                "description": description,
                "reason": reason,
            }],
            "resource_type": "gts.cf.core.events.request.v1~",
        },
    })
}

#[tokio::test]
async fn publish_event_rejects_a_non_printable_source() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "orders\nservice",
            "subject": "s1",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json(),
        envelope_violation(
            "source",
            "must contain only printable ASCII (0x20-0x7E)",
            "ascii_only",
        )
    );
}

#[tokio::test]
async fn publish_event_rejects_an_over_length_subject() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": "orders-service",
            "subject": "s".repeat(1025),
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json(),
        envelope_violation(
            "subject",
            "must be 1-1024 bytes, got 1025",
            "field_too_long"
        )
    );
}

#[tokio::test]
async fn publish_event_envelope_encoding_is_checked_before_event_type_lookup() {
    // No type registry, so the type below is unregistered either way.
    let harness = EventBrokerHarness::builder().build().await;

    let body = |source: &str| {
        json!({
            "id": Uuid::new_v4(),
            "type": "gts.cf.core.events.event.v1~x.eb.t1.unregistered.v1~",
            "tenant_id": Uuid::new_v4(),
            "source": source,
            "subject": "s1",
            "subject_type": "gts.x.eb.t1.subject.v1~",
            "occurred_at": Utc::now().to_rfc3339(),
        })
    };

    // Control: with a well-formed envelope the same publish fails on the
    // lookup, which is what makes the assertion below meaningful rather than
    // vacuous - the type really is unregistered.
    let control = harness
        .api_v1()
        .post_events()
        .with_body(Json(&body("orders-service")))
        .send()
        .await;
    assert_eq!(
        control.json()["status"],
        json!(404),
        "the control publish must fail on event-type lookup, got {}",
        control.json()
    );

    // A mis-encoded `source` on the same unregistered type reports the
    // encoding failure, because the envelope check precedes the lookup.
    let resp = harness
        .api_v1()
        .post_events()
        .with_body(Json(&body("orders\u{1}service")))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json(),
        envelope_violation(
            "source",
            "must contain only printable ASCII (0x20-0x7E)",
            "ascii_only",
        )
    );
}
