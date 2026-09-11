//! `handlers/delivery/subscriptions.rs` coverage (`eb-rest-handlers` task
//! 11.5): JOIN validation matrix, authz enforcement, list, get, leave, seek.
//! `:stream`/`:sse` frame + lifecycle coverage lives in `streaming_tests.rs`
//! instead (split out per `REVIEW-TODO.md` item #12).
//!
//! Every test inlines its own setup (seeding a topic, creating a consumer
//! group, joining a subscription) and asserts the exact response body
//! received - no shared setup helpers, so nothing about what a test sends,
//! or what it checks, is hidden outside the test itself. Dynamic
//! server-minted values (subscription id, consumer-group id, `expires_at`)
//! are extracted from the response and reused to build the exact expected
//! body, rather than skipped.
//!
//! One test below still drives `harness.router()` directly with
//! `tower::ServiceExt` rather than `RequestCase::send()` (which eagerly
//! collects the *entire* response body via `axum::body::to_bytes` - fine for
//! ordinary JSON responses, but hangs on `:stream`'s intentionally
//! long-lived body): `stream_seek_and_leave_each_make_their_own_fresh_tenant_check`
//! needs a real `:stream` response to prove its tenant check runs on a live
//! stream open, matching `streaming_tests.rs`'s same raw-router pattern.

use std::sync::Arc;

use authz_resolver_sdk::{EvaluationRequest, PolicyEnforcer};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use serde_json::json;
use tokio_stream::StreamExt;
use toolkit_gts::GtsInstanceId;
use toolkit_security::pep_properties;
use tower::ServiceExt;
use uuid::Uuid;

use crate::domain::model::{ConsumerGroup, ConsumerGroupKind};
use crate::domain::repo::{ConsumerGroupRepo, CursorRepo, SubscriptionRepo};
use crate::domain::streaming::lease::StreamLeases;
use crate::test_support::{DenyingAuthZ, EventBrokerHarness, Json, StaticTypesRegistry};

// -- JOIN validation matrix --

#[tokio::test]
async fn join_happy_path_returns_201_with_full_assignment() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 3 },
        ])))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tenant_id = Uuid::new_v4();

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": tenant_id,
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await;

    resp.assert_status(201);
    let body = resp.json();
    let id = body["id"].as_str().expect("id must be a string").to_owned();
    let expires_at = body["expires_at"]
        .as_str()
        .expect("expires_at must be a string")
        .to_owned();
    assert_eq!(
        body,
        json!({
            "id": id,
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": tenant_id,
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
                "max_depth": 0,
                "barrier_mode": "respect",
                "filter": null,
            }],
            "assigned": [
                { "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0 },
                { "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 1 },
                { "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 2 },
            ],
            "topology_version": 1,
            "expires_at": expires_at,
        })
    );
}

/// The tenant-traversal knobs are carried through JOIN and echoed, even though
/// fan-out does not expand them yet. Before they reached the domain model the
/// request accepted them and the conversion dropped them, so a caller asking
/// for unlimited descendants got `201` and no widening at all - a silent
/// mismatch between what was requested and what was stored.
#[tokio::test]
async fn join_carries_tenant_traversal_scope_through_to_the_echoed_interest() {
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
    let tenant_id = Uuid::new_v4();

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": tenant_id,
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
                "max_depth": null,
                "barrier_mode": "ignore",
            }],
        })))
        .send()
        .await;

    resp.assert_status(201);
    let body = resp.json();
    let id = body["id"].as_str().expect("id must be a string").to_owned();
    let expires_at = body["expires_at"]
        .as_str()
        .expect("expires_at must be a string")
        .to_owned();
    assert_eq!(
        body,
        json!({
            "id": id,
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": tenant_id,
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
                "max_depth": null,
                "barrier_mode": "ignore",
                "filter": null,
            }],
            "assigned": [
                { "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0 },
            ],
            "topology_version": 1,
            "expires_at": expires_at,
        })
    );
}

/// An absent `max_depth` and an explicit `null` are opposite ends of the
/// scope range, so the echo has to distinguish them: this asserts the narrow
/// end, against the wide end above.
#[tokio::test]
async fn join_echoes_an_absent_max_depth_as_the_current_tenant_not_as_unlimited() {
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await;

    resp.assert_status(201);
    assert_eq!(resp.json()["interests"][0]["max_depth"], json!(0));
}

/// The third shape through the untagged `max_depth` enum: a positive integer
/// has to reach the `Levels` variant rather than being swallowed by the `null`
/// variant that precedes it.
#[tokio::test]
async fn join_carries_a_positive_max_depth_through_as_that_many_levels() {
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
                "max_depth": 2,
            }],
        })))
        .send()
        .await;

    resp.assert_status(201);
    assert_eq!(resp.json()["interests"][0]["max_depth"], json!(2));
}

#[tokio::test]
async fn join_rejects_an_unrecognised_barrier_mode() {
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
                "barrier_mode": "traverse",
            }],
        })))
        .send()
        .await;

    // An unknown enum variant fails at deserialization, before the handler
    // body runs - a plain-text 422, matching how `ProducerModeDto` already
    // rejects an unknown producer mode. The trailing "at line N column M"
    // offset is deliberately not asserted: it is a `serde` formatting detail
    // that has been seen to differ between otherwise-identical builds.
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
            "instance": "/event-broker/v1/subscriptions",
            "context": {},
        })
    );
}

#[tokio::test]
async fn join_rejects_a_negative_max_depth() {
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
                "max_depth": -1,
            }],
        })))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json()["detail"],
        json!("InvalidBody: 'max_depth' must be >= 0 or null, got -1")
    );
}

#[tokio::test]
async fn join_unregistered_consumer_group_returns_404() {
    let harness = EventBrokerHarness::builder().build().await;

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": "gts.cf.core.events.consumer_group.v1~x.eb.t1.missing.v1",
            "client_agent": "test-agent",
            "interests": [],
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
            "detail": "consumer group 'gts.cf.core.events.consumer_group.v1~x.eb.t1.missing.v1' is not \
                       registered - anonymous groups must be created via POST \
                       /v1/consumer-groups first",
            "instance": "/event-broker/v1/subscriptions",
            "context": {
                "resource_name": "gts.cf.core.events.consumer_group.v1~x.eb.t1.missing.v1",
                "resource_type": "gts.cf.core.events.consumer_group.v1~",
            },
        })
    );
}

#[tokio::test]
async fn join_unregistered_topic_returns_404() {
    let harness = EventBrokerHarness::builder().build().await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.missing.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
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
            "instance": "/event-broker/v1/subscriptions",
            "context": {
                "resource_name": "gts.cf.core.events.topic.v1~x.eb.missing.topic.v1",
                "resource_type": "gts.cf.core.events.topic.v1~",
            },
        })
    );
}

#[tokio::test]
async fn join_bad_type_pattern_returns_400() {
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            // Two wildcard segments - violates GTS §10 (at most one
            // wildcard segment may appear in a pattern).
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.*.foo.*"],
            }],
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
            "detail": "BadTypePattern: 'gts.*.foo.*' violates GTS wildcard rules - a wildcard \
                       must fill its whole segment and at most one segment may be a wildcard",
            "instance": "/event-broker/v1/subscriptions",
            "context": {
                "format": "BadTypePattern: 'gts.*.foo.*' violates GTS wildcard rules - a \
                           wildcard must fill its whole segment and at most one segment may be \
                           a wildcard",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

// -- session_timeout parsing (toolkit_utils::iso8601_duration) --

/// Seconds between `expires_at` (RFC 3339) and now - used to check the
/// parsed `session_timeout` actually took effect, since `SubscriptionDto`
/// doesn't echo `session_timeout` itself.
fn expires_at_secs_from_now(body: &serde_json::Value) -> i64 {
    let expires_at = body["expires_at"]
        .as_str()
        .expect("expires_at must be a string");
    let expires_at = chrono::DateTime::parse_from_rfc3339(expires_at).expect("valid RFC 3339");
    (expires_at.with_timezone(&Utc) - Utc::now()).num_seconds()
}

#[tokio::test]
async fn join_without_session_timeout_defaults_to_30_seconds() {
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await;

    resp.assert_status(201);
    assert!(
        (25..=30).contains(&expires_at_secs_from_now(&resp.json())),
        "expected ~30s default session timeout"
    );
}

#[tokio::test]
async fn join_session_timeout_with_hour_designator_is_parsed_correctly() {
    // The old hand-rolled parser only understood `PT<n>S`/`PT<n>M` and
    // would have silently defaulted `PT1H` to 30 seconds - this proves the
    // full hour is actually honored.
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "session_timeout": "PT1H",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await;

    resp.assert_status(201);
    assert!(
        (3590..=3600).contains(&expires_at_secs_from_now(&resp.json())),
        "expected the full 1-hour session timeout, not the 30s default"
    );
}

#[tokio::test]
async fn join_malformed_session_timeout_returns_400_not_silently_defaulted() {
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "session_timeout": "garbage",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
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
            "detail": "InvalidSessionTimeout: 'garbage' is not a valid ISO 8601 duration: \
                       duration must start with 'P'",
            "instance": "/event-broker/v1/subscriptions",
            "context": {
                "format": "InvalidSessionTimeout: 'garbage' is not a valid ISO 8601 duration: \
                           duration must start with 'P'",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn join_session_timeout_missing_pt_prefix_returns_400() {
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "session_timeout": "30S",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json()["detail"],
        "InvalidSessionTimeout: '30S' is not a valid ISO 8601 duration: duration must start \
         with 'P'"
    );
}

#[tokio::test]
async fn join_zero_session_timeout_returns_400() {
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "session_timeout": "PT0S",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json()["detail"],
        "InvalidSessionTimeout: session_timeout must be a positive duration, got 'PT0S'"
    );
}

#[tokio::test]
async fn join_negative_session_timeout_returns_400() {
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "session_timeout": "-PT30S",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json()["detail"],
        "InvalidSessionTimeout: '-PT30S' is not a valid ISO 8601 duration: a signed duration \
         is not supported (std::time::Duration is unsigned)"
    );
}

#[tokio::test]
async fn join_session_timeout_overflow_returns_400_not_a_1_second_timeout() {
    // The old hand-rolled parser's minutes-to-seconds conversion silently
    // overflowed to "0" on a large enough value, which then clamped to a
    // **1-second** session timeout - the opposite of a safe fallback. This
    // proves that shape is now a loud 400, not a silently-accepted
    // near-instant expiry.
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "session_timeout": format!("PT{}M", u64::MAX),
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await;

    resp.assert_status(400);
    assert_eq!(
        resp.json()["context"]["resource_type"],
        "gts.cf.core.events.request.v1~"
    );
}

// -- authz enforcement (`gears-rust#4516`, `eb-authz-enforcement`) --

#[tokio::test]
async fn join_rejected_for_missing_topic_consume_permission_returns_403() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: |req: &EvaluationRequest| req.resource.properties.contains_key("topic_id"),
        })))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
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
            "instance": "/event-broker/v1/subscriptions",
            "context": {
                "reason": "access denied by policy",
                "resource_type": "gts.cf.core.events.topic.v1~",
            },
        })
    );
    assert_eq!(
        harness.api_v1().get_subscriptions().send().await.json()["items"],
        json!([]),
        "no subscription must be created for a denied JOIN"
    );
}

#[tokio::test]
async fn join_rejected_for_missing_event_type_consume_permission_returns_403_naming_the_offending_type()
 {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: |req: &EvaluationRequest| {
                req.resource.properties.get("event_type_id")
                    == Some(&json!("gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"))
            },
        })))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
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
            "instance": "/event-broker/v1/subscriptions",
            "context": {
                "reason": "calling principal lacks consume on event type \
                           'gts.cf.core.events.event.v1~x.eb.t1.foo.v1~'",
                "resource_type": "gts.cf.core.events.event_type.v1~",
            },
        })
    );
}

#[tokio::test]
async fn join_rejected_for_unauthorized_interest_tenant_id_returns_403() {
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
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
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
            "instance": "/event-broker/v1/subscriptions",
            "context": {
                "reason": "access denied by policy",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn join_rejected_for_a_caller_not_authorized_for_the_anonymous_groups_tenant() {
    // Empty `interests`, so the only tenant-scope check JOIN can possibly
    // make is its own group-level one (`eb-tenant-isolation-fix`) against
    // the anonymous group's tenant - distinct from
    // `join_rejected_for_unauthorized_interest_tenant_id_returns_403`,
    // which denies the *per-interest* tenant check instead.
    let harness = EventBrokerHarness::builder()
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: |req: &EvaluationRequest| {
                req.resource
                    .properties
                    .contains_key(pep_properties::OWNER_TENANT_ID)
            },
        })))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [],
        })))
        .send()
        .await;

    resp.assert_status(403);
}

#[tokio::test]
async fn join_rejected_for_a_named_group_without_consume_permission() {
    let harness = EventBrokerHarness::builder()
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: |req: &EvaluationRequest| {
                req.resource.properties.contains_key("consumer_group_id")
            },
        })))
        .build()
        .await;
    let group_id = "gts.cf.core.events.consumer_group.v1~x.eb.t1.namedgroup.v1";
    harness
        .repo()
        .create_consumer_group(ConsumerGroup {
            id: GtsInstanceId::try_new(group_id)
                .expect("test-seeded id must be a valid GTS instance id"),
            kind: ConsumerGroupKind::Named,
            tenant_id: Uuid::new_v4(),
            owner_principal_id: Uuid::new_v4(),
            description: None,
            created_at: Utc::now(),
        })
        .await
        .expect("seeding a named consumer group must not fail");

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group_id,
            "client_agent": "test-agent",
            "interests": [],
        })))
        .send()
        .await;

    resp.assert_status(403);
}

#[tokio::test]
async fn join_a_single_unauthorized_interest_blocks_the_whole_join() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            { "id": "gts.cf.core.events.topic.v1~x.eb.t2.topic.v1", "partitions": 1 },
        ])))
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            // The second interest's topic is denied; the first interest's
            // topic (and both interests' event-type checks) would pass.
            deny_if: |req: &EvaluationRequest| {
                req.resource.properties.get("topic_id")
                    == Some(&json!("gts.cf.core.events.topic.v1~x.eb.t2.topic.v1"))
            },
        })))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [
                {
                    "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                    "tenant_id": Uuid::new_v4(),
                    "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
                },
                {
                    "topic": "gts.cf.core.events.topic.v1~x.eb.t2.topic.v1",
                    "tenant_id": Uuid::new_v4(),
                    "types": ["gts.cf.core.events.event.v1~x.eb.t2.foo.v1~"],
                },
            ],
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
            "instance": "/event-broker/v1/subscriptions",
            "context": {
                "reason": "access denied by policy",
                "resource_type": "gts.cf.core.events.topic.v1~",
            },
        })
    );
    assert_eq!(
        harness.api_v1().get_subscriptions().send().await.json()["items"],
        json!([]),
        "no subscription must be created for either interest - not even the one that would have passed"
    );
}

#[tokio::test]
async fn join_succeeds_when_every_interest_passes_all_three_checks() {
    let interest_tenant_id = Uuid::new_v4();
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            // Pins the exact topic/event-type this test joins with - proves
            // the real values get checked, not a hardcoded pass. Any
            // tenant-scope check is allowed regardless of which tenant it
            // names: JOIN now makes two of them (the group-level check
            // against the anonymous group's own tenant, and the
            // per-interest check against `interest_tenant_id`), and this
            // test isn't about pinning either specific value.
            // `create_consumer_group`'s own `consumer_group:define` check
            // (`eb-tenant-isolation-fix`) has no resource properties at all,
            // so it's allowed by action name instead.
            deny_if: move |req: &EvaluationRequest| {
                let allowed_topic = req.resource.properties.get("topic_id")
                    == Some(&json!("gts.cf.core.events.topic.v1~x.eb.t1.topic.v1"));
                let allowed_type = req.resource.properties.get("event_type_id")
                    == Some(&json!("gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"));
                let allowed_tenant = req
                    .resource
                    .properties
                    .contains_key(pep_properties::OWNER_TENANT_ID);
                let allowed_define = req.action.name == "define";
                !(allowed_topic || allowed_type || allowed_tenant || allowed_define)
            },
        })))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": interest_tenant_id,
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await;

    resp.assert_status(201);
    let body = resp.json();
    let id = body["id"].as_str().expect("id must be a string").to_owned();
    let expires_at = body["expires_at"]
        .as_str()
        .expect("expires_at must be a string")
        .to_owned();
    assert_eq!(
        body,
        json!({
            "id": id,
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": interest_tenant_id,
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
                "max_depth": 0,
                "barrier_mode": "respect",
                "filter": null,
            }],
            "assigned": [
                { "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0 },
            ],
            "topology_version": 1,
            "expires_at": expires_at,
        })
    );
}

#[tokio::test]
async fn stream_seek_and_leave_each_make_their_own_fresh_tenant_check() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls_for_double = Arc::clone(&calls);
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            // `join()` makes exactly 4 PEP calls here: its own group-level
            // tenant-scope check against the anonymous group it joins
            // (`eb-tenant-isolation-fix`'s group-level JOIN authorization),
            // plus the one interest's `topic:consume`, `event_type:consume`,
            // and per-interest tenant-scope check. `stream`/`seek`/`leave`
            // each now make one more of their own
            // (`find_authorized_subscription`) - never zero (the pre-fix
            // behavior this test used to lock in) and never a reuse of
            // JOIN's own checks (spec.md "A passed check does not constrain
            // later reads" is about not caching JOIN's `AccessScope`, not
            // about skipping authorization downstream entirely - each call
            // below is its own fresh tenant-scope check). Never denies, so
            // every operation still succeeds; only the call count matters.
            deny_if: move |_: &EvaluationRequest| {
                calls_for_double.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                false
            },
        })))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let join_resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
        })))
        .send()
        .await;
    join_resp.assert_status(201);
    let id = join_resp.json()["id"].as_str().unwrap().to_owned();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 5);

    harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0, "value": "earliest" }]
        })))
        .send()
        .await
        .assert_status(200);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        6,
        "seek must make exactly one additional tenant check"
    );

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
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        7,
        "stream must make exactly one additional tenant check"
    );
    let mut stream = response.into_body().into_data_stream();
    tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("must receive the first frame within 2s")
        .expect("stream must not end before the first frame")
        .expect("first chunk must not be an error");
    drop(stream);

    let delete_resp = harness.api_v1().delete_subscription(&id).send().await;
    delete_resp.assert_status(204);

    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        8,
        "leave must make exactly one additional tenant check"
    );
}

/// Joins a subscription (with one real topic interest, so it has an
/// assigned partition a `seek` target can address) using a
/// `PolicyEnforcer` that allows exactly the 2 PEP calls setup makes - the
/// anonymous group's own `consumer_group:define` check
/// (`create_consumer_group`) and `join`'s group-level tenant-scope check
/// against that group (empty `interests`, so no per-interest checks) - and
/// denies every call after. Simulates a caller who was authorized to create
/// the group and subscription but is not authorized for its tenant on a
/// later call, the same technique
/// `stream_seek_and_leave_each_make_their_own_fresh_tenant_check` uses.
/// Returns the harness, the created subscription's id, and its consumer
/// group.
async fn harness_with_subscription_then_deny_further_tenant_checks()
-> (EventBrokerHarness, String, String) {
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
            { "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~", "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1" },
        ])))
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: move |_: &EvaluationRequest| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2
            },
        })))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let join_resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "test-agent",
            // Empty on purpose: an interest here would make its own
            // tenant-scope check during JOIN, which `deny_if` (denying any
            // tenant-scope check) would also catch, breaking JOIN itself.
            // `seek`'s target topic/partition doesn't need to be one of the
            // subscription's own interests (see `DeliveryServiceImpl::seek`),
            // so an empty interests list plus the seeded topic above is
            // enough for every test using this helper.
            "interests": [],
        })))
        .send()
        .await;
    join_resp.assert_status(201);
    let id = join_resp.json()["id"].as_str().unwrap().to_owned();
    (harness, id, group)
}

#[tokio::test]
async fn get_subscription_rejects_a_caller_from_a_different_tenant() {
    let (harness, id, _group) = harness_with_subscription_then_deny_further_tenant_checks().await;

    let resp = harness.api_v1().get_subscription(&id).send().await;

    resp.assert_status(403);
}

#[tokio::test]
async fn leave_rejects_a_caller_from_a_different_tenant() {
    let (harness, id, _group) = harness_with_subscription_then_deny_further_tenant_checks().await;

    let resp = harness.api_v1().delete_subscription(&id).send().await;

    resp.assert_status(403);
    assert!(
        harness
            .repo()
            .find_subscription(Uuid::parse_str(&id).unwrap())
            .await
            .expect("repo lookup must not fail")
            .is_some(),
        "a denied leave must not delete the subscription"
    );
}

#[tokio::test]
async fn seek_rejects_a_caller_from_a_different_tenant() {
    let (harness, id, group) = harness_with_subscription_then_deny_further_tenant_checks().await;
    let topic = toolkit_gts::GtsInstanceId::try_new("gts.cf.core.events.topic.v1~x.eb.t1.topic.v1")
        .unwrap();

    let resp = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "partition": 0,
                "value": "earliest",
            }],
        })))
        .send()
        .await;

    resp.assert_status(403);
    assert!(
        harness
            .repo()
            .find_cursor(&GtsInstanceId::try_new(&group).unwrap(), &topic, 0)
            .await
            .expect("repo lookup must not fail")
            .is_none(),
        "a denied seek must not write a cursor"
    );
}

#[tokio::test]
async fn stream_rejects_a_caller_from_a_different_tenant() {
    let (harness, id, _group) = harness_with_subscription_then_deny_further_tenant_checks().await;

    let resp = harness.api_v1().get_events_stream(&id).send().await;

    resp.assert_status(403);
    assert!(
        !harness.leases().is_held(Uuid::parse_str(&id).unwrap()),
        "a denied stream open must not take the subscription's stream lease - \
         the lease is acquired after the authz check, so a 403 never reaches it"
    );
}

#[tokio::test]
async fn list_subscriptions_excludes_a_different_tenant() {
    // `AllowAllAuthZ` (the harness default) can't distinguish tenants at
    // all, so this needs a `PolicyEnforcer` that actually denies one
    // specific foreign tenant by value while allowing everything else
    // (including the harness's own tenant-scope checks).
    let foreign_tenant_id = Uuid::new_v4();
    let harness = EventBrokerHarness::builder()
        .with_policy_enforcer(PolicyEnforcer::new(Arc::new(DenyingAuthZ {
            deny_if: move |req: &EvaluationRequest| {
                req.resource.properties.get(pep_properties::OWNER_TENANT_ID)
                    == Some(&json!(foreign_tenant_id.to_string()))
            },
        })))
        .build()
        .await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let own = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(
            &json!({ "consumer_group": group, "client_agent": "own", "interests": [] }),
        ))
        .send()
        .await;
    own.assert_status(201);
    let own_id = own.json()["id"].as_str().unwrap().to_owned();

    // A subscription belonging to a different tenant, inserted directly
    // into the repo (there's no REST path to create one under a tenant the
    // harness's own `ctx` doesn't hold - that's exactly the property this
    // test is checking).
    let foreign = crate::domain::model::Subscription {
        id: Uuid::new_v4(),
        tenant_id: foreign_tenant_id,
        consumer_group: GtsInstanceId::try_new(&group).unwrap(),
        client_agent: "foreign".to_owned(),
        interests: vec![],
        topics: vec![],
        assigned: vec![],
        topology_version: 1,
        session_timeout: std::time::Duration::from_secs(30),
        last_seen_at: Utc::now(),
        expires_at: Utc::now() + chrono::Duration::seconds(30),
    };
    harness
        .repo()
        .put_subscription(&foreign)
        .await
        .expect("seeding a foreign-tenant subscription must not fail");

    let resp = harness.api_v1().get_subscriptions().send().await;

    resp.assert_status(200);
    let ids: Vec<String> = resp.json()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, vec![own_id]);
}

// -- list / get / leave --

#[tokio::test]
async fn list_subscriptions_happy_path_and_filter_by_consumer_group() {
    let harness = EventBrokerHarness::builder().build().await;
    let group_a = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let group_b = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let joined_a = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(
            &json!({ "consumer_group": group_a, "client_agent": "a", "interests": [] }),
        ))
        .send()
        .await;
    joined_a.assert_status(201);
    let joined_b = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(
            &json!({ "consumer_group": group_b, "client_agent": "b", "interests": [] }),
        ))
        .send()
        .await;
    joined_b.assert_status(201);

    let all = harness.api_v1().get_subscriptions().send().await;
    all.assert_status(200);
    let mut all_body = all.json();
    all_body["items"]
        .as_array_mut()
        .unwrap()
        .sort_by_key(|s| s["id"].as_str().unwrap().to_owned());
    let mut expected_items = vec![joined_a.json(), joined_b.json()];
    expected_items.sort_by_key(|s| s["id"].as_str().unwrap().to_owned());
    assert_eq!(
        all_body,
        json!({
            "items": expected_items,
            "page_info": { "next_cursor": null, "prev_cursor": null, "limit": 25 },
        })
    );

    let filtered = harness
        .api_v1()
        .get_subscriptions()
        .with_query("$filter", format!("consumer_group%20eq%20'{group_a}'"))
        .send()
        .await;
    filtered.assert_status(200);
    assert_eq!(
        filtered.json(),
        json!({
            "items": [joined_a.json()],
            "page_info": { "next_cursor": null, "prev_cursor": null, "limit": 25 },
        })
    );
}

#[tokio::test]
async fn get_subscription_happy_path_and_404() {
    let harness = EventBrokerHarness::builder().build().await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let joined = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(
            &json!({ "consumer_group": group, "client_agent": "a", "interests": [] }),
        ))
        .send()
        .await;
    let id = joined.json()["id"].as_str().unwrap().to_owned();

    let found = harness.api_v1().get_subscription(&id).send().await;
    found.assert_status(200);
    assert_eq!(found.json(), joined.json());

    let missing_id = Uuid::new_v4();
    let not_found = harness
        .api_v1()
        .get_subscription(&missing_id.to_string())
        .send()
        .await;
    not_found.assert_status(404);
    assert_eq!(
        not_found.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": format!("subscription '{missing_id}' does not exist"),
            "instance": format!("/event-broker/v1/subscriptions/{missing_id}"),
            "context": {
                "resource_name": missing_id.to_string(),
                "resource_type": "gts.cf.core.events.subscription.v1~",
            },
        })
    );
}

#[tokio::test]
async fn leave_subscription_happy_path_and_404() {
    let harness = EventBrokerHarness::builder().build().await;
    let group = harness.api_v1().post_consumer_groups().send().await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let joined = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(
            &json!({ "consumer_group": group, "client_agent": "a", "interests": [] }),
        ))
        .send()
        .await;
    let id = joined.json()["id"].as_str().unwrap().to_owned();

    let delete_resp = harness.api_v1().delete_subscription(&id).send().await;
    delete_resp.assert_status(204);
    assert_eq!(delete_resp.text(), "", "204 No Content must carry no body");

    let get_after_delete = harness.api_v1().get_subscription(&id).send().await;
    get_after_delete.assert_status(404);
    assert_eq!(
        get_after_delete.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": format!("subscription '{id}' does not exist"),
            "instance": format!("/event-broker/v1/subscriptions/{id}"),
            "context": {
                "resource_name": id,
                "resource_type": "gts.cf.core.events.subscription.v1~",
            },
        })
    );

    let missing_id = Uuid::new_v4();
    let delete_missing = harness
        .api_v1()
        .delete_subscription(&missing_id.to_string())
        .send()
        .await;
    delete_missing.assert_status(404);
    assert_eq!(
        delete_missing.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.not_found.v1~",
            "title": "Not Found",
            "status": 404,
            "detail": format!("subscription '{missing_id}' does not exist"),
            "instance": format!("/event-broker/v1/subscriptions/{missing_id}"),
            "context": {
                "resource_name": missing_id.to_string(),
                "resource_type": "gts.cf.core.events.subscription.v1~",
            },
        })
    );
}

// -- seek --

/// An exact offset inside the partition's admissible range is stored and
/// echoed back.
///
/// Six events are published first: the range a partition admits ends at the
/// highest sequence it has assigned, so `5` is only a position a cursor may
/// hold once there is something up there. This test asserted `200` against an
/// empty partition before the broker validated the range at all.
#[tokio::test]
async fn seek_exact_offset_returns_200_with_resolved_value() {
    let topic = "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1";
    let tenant_id = Uuid::new_v4();
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": topic, "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": topic,
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
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
                "topic": topic,
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
    publish_and_settle(&harness, &id, topic, tenant_id, 6).await;

    let resp = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": topic, "partition": 0, "value": 5 }]
        })))
        .send()
        .await;

    resp.assert_status(200);
    assert_eq!(
        resp.json(),
        json!([{ "topic": topic, "partition": 0, "value": 5 }])
    );
}

#[tokio::test]
async fn seek_earliest_sentinel_resolves_to_zero_on_empty_partition() {
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

    let resp = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0, "value": "earliest" }]
        })))
        .send()
        .await;

    resp.assert_status(200);
    assert_eq!(
        resp.json(),
        json!([{ "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0, "value": 0 }])
    );
}

#[tokio::test]
async fn seek_invalid_sentinel_returns_400() {
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

    let resp = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0, "value": "not-a-sentinel" }]
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
            "detail": "InvalidSeekValue: 'not-a-sentinel' is not a valid SEEK value - expected \
                       an integer, \"earliest\", \"latest\", or \"at:<ISO-8601>\"",
            "instance": format!("/event-broker/v1/subscriptions/{id}:seek"),
            "context": {
                "format": "InvalidSeekValue: 'not-a-sentinel' is not a valid SEEK value - \
                           expected an integer, \"earliest\", \"latest\", or \"at:<ISO-8601>\"",
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn seek_unknown_subscription_returns_404() {
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partitions": 1 },
        ])))
        .build()
        .await;
    let missing_id = Uuid::new_v4();

    let resp = harness
        .api_v1()
        .post_subscription_seek(&missing_id.to_string())
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1", "partition": 0, "value": 0 }]
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
            "detail": format!("subscription '{missing_id}' does not exist"),
            "instance": format!("/event-broker/v1/subscriptions/{missing_id}:seek"),
            "context": {
                "resource_name": missing_id.to_string(),
                "resource_type": "gts.cf.core.events.subscription.v1~",
            },
        })
    );
}

#[tokio::test]
async fn seek_unregistered_topic_returns_404() {
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

    let resp = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": "gts.cf.core.events.topic.v1~x.eb.missing.topic.v1", "partition": 0, "value": 0 }]
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
            "instance": format!("/event-broker/v1/subscriptions/{id}:seek"),
            "context": {
                "resource_name": "gts.cf.core.events.topic.v1~x.eb.missing.topic.v1",
                "resource_type": "gts.cf.core.events.topic.v1~",
            },
        })
    );
}

/// Waits until the partition's high-water mark has reached `expected`, by
/// asking the broker to resolve `"latest"`.
///
/// Publishing is accepted with `202` and the sequence is assigned afterwards by
/// the ingest outbox, so a test that needs a known ceiling has to wait for one
/// rather than assume it. Nothing about a request or an assertion is hidden
/// here: this only decides *when* the test may proceed.
async fn await_hwm(harness: &EventBrokerHarness, subscription: &str, topic: &str, expected: i64) {
    for _ in 0..300 {
        let resp = harness
            .api_v1()
            .post_subscription_seek(subscription)
            .with_body(Json(&json!({
                "partition_positions": [{ "topic": topic, "partition": 0, "value": "latest" }]
            })))
            .send()
            .await;
        if resp.json() == json!([{ "topic": topic, "partition": 0, "value": expected }]) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the high-water mark never reached {expected}");
}

/// Publishes `count` events onto partition 0 of the single-partition test
/// topic and returns once every one of them has a sequence.
async fn publish_and_settle(
    harness: &EventBrokerHarness,
    subscription: &str,
    topic: &str,
    tenant_id: Uuid,
    count: i64,
) {
    for n in 0..count {
        harness
            .api_v1()
            .post_events()
            .with_body(Json(&json!({
                "id": Uuid::new_v4(),
                "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "tenant_id": tenant_id,
                "source": "test-seek-range",
                "subject": format!("s{n}"),
                "subject_type": "gts.x.eb.t1.subject.v1~",
                "occurred_at": chrono::Utc::now().to_rfc3339(),
            })))
            .send()
            .await
            .assert_status(202);
    }
    await_hwm(harness, subscription, topic, count).await;
}

/// The range's own edges: the floor and the ceiling are both positions a cursor
/// may hold, and one step outside either is not.
///
/// A populated partition that retention has not touched has a floor of `0` -
/// the position below its oldest surviving event - and a ceiling of the highest
/// sequence assigned.
#[tokio::test]
async fn seek_accepts_the_range_edges_and_rejects_one_step_outside_either() {
    let topic = "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1";
    let tenant_id = Uuid::new_v4();
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": topic, "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": topic,
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
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
                "topic": topic,
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
    publish_and_settle(&harness, &id, topic, tenant_id, 3).await;

    let floor = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": topic, "partition": 0, "value": 0 }]
        })))
        .send()
        .await;
    floor.assert_status(200);
    assert_eq!(
        floor.json(),
        json!([{ "topic": topic, "partition": 0, "value": 0 }])
    );

    let ceiling = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": topic, "partition": 0, "value": 3 }]
        })))
        .send()
        .await;
    ceiling.assert_status(200);
    assert_eq!(
        ceiling.json(),
        json!([{ "topic": topic, "partition": 0, "value": 3 }])
    );

    let below = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": topic, "partition": 0, "value": -1 }]
        })))
        .send()
        .await;
    below.assert_status(400);
    assert_eq!(
        below.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "Request validation failed",
            "instance": format!("/event-broker/v1/subscriptions/{id}:seek"),
            "context": {
                "field_violations": [{
                    "field": "partition_positions[0].value",
                    "description": "the position is below the valid range [0, 3]",
                    "reason": "below_retention_floor",
                }],
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );

    let above = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": topic, "partition": 0, "value": 4 }]
        })))
        .send()
        .await;
    above.assert_status(400);
    assert_eq!(
        above.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "Request validation failed",
            "instance": format!("/event-broker/v1/subscriptions/{id}:seek"),
            "context": {
                "field_violations": [{
                    "field": "partition_positions[0].value",
                    "description": "the position is above the valid range [0, 3]",
                    "reason": "above_high_water_mark",
                }],
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

/// A request naming several partitions applies all of them or none.
///
/// Asserted by reading the cursors afterwards rather than inferred from the
/// status code: a `400` with one cursor written would look identical from
/// outside.
#[tokio::test]
async fn seek_writes_no_cursor_at_all_when_one_partition_is_inadmissible() {
    let topic = "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1";
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": topic, "partitions": 2 },
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
                "topic": topic,
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

    // Nothing has been published, so both partitions admit only `0`. The first
    // entry is admissible and the second is not.
    let resp = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [
                { "topic": topic, "partition": 0, "value": 0 },
                { "topic": topic, "partition": 1, "value": 5 },
            ]
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
            "detail": "Request validation failed",
            "instance": format!("/event-broker/v1/subscriptions/{id}:seek"),
            "context": {
                "field_violations": [{
                    "field": "partition_positions[1].value",
                    "description": "the position is above the valid range [0, 0]",
                    "reason": "above_high_water_mark",
                }],
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );

    let topic_id = GtsInstanceId::try_new(topic).unwrap();
    let group_id = GtsInstanceId::try_new(&group).unwrap();
    for partition in [0, 1] {
        assert!(
            harness
                .repo()
                .find_cursor(&group_id, &topic_id, partition)
                .await
                .expect("repo lookup must not fail")
                .is_none(),
            "partition {partition} must have no cursor: the rejected request \
             applies nothing",
        );
    }
}

/// Every offending entry is reported, not just the first, so a caller
/// correcting several positions needs one round trip rather than one per
/// partition.
#[tokio::test]
async fn seek_reports_one_violation_per_offending_partition() {
    let topic = "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1";
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": topic, "partitions": 3 },
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
                "topic": topic,
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

    let resp = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [
                { "topic": topic, "partition": 0, "value": 7 },
                { "topic": topic, "partition": 1, "value": 0 },
                { "topic": topic, "partition": 2, "value": -3 },
            ]
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
            "detail": "Request validation failed",
            "instance": format!("/event-broker/v1/subscriptions/{id}:seek"),
            "context": {
                "field_violations": [
                    {
                        "field": "partition_positions[0].value",
                        "description": "the position is above the valid range [0, 0]",
                        "reason": "above_high_water_mark",
                    },
                    {
                        "field": "partition_positions[2].value",
                        "description": "the position is below the valid range [0, 0]",
                        "reason": "below_retention_floor",
                    },
                ],
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

/// A cursor names a position, not an event, and the position it names may be
/// one retention has erased.
///
/// Two events are published, a cutoff is taken, and two more follow; a
/// retention pass bounded by that cutoff removes sequences 1 and 2. The floor
/// is then 2 - the position below the oldest surviving event, which is exactly
/// the event that was removed - so a cursor may sit there, and a stream opened
/// on it delivers the events already stored above it.
#[tokio::test]
async fn a_cursor_may_sit_on_an_erased_position_and_delivery_resumes_above_it() {
    let topic = "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1";
    let tenant_id = Uuid::new_v4();
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": topic, "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": topic,
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
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
                "topic": topic,
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

    publish_and_settle(&harness, &id, topic, tenant_id, 2).await;
    // Retention bounds by the time the broker assigned the sequence, so the
    // cutoff has to fall between the two bursts.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let cutoff = chrono::Utc::now();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    for n in 2..4 {
        harness
            .api_v1()
            .post_events()
            .with_body(Json(&json!({
                "id": Uuid::new_v4(),
                "type": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "tenant_id": tenant_id,
                "source": "test-erased-cursor",
                "subject": format!("s{n}"),
                "subject_type": "gts.x.eb.t1.subject.v1~",
                "occurred_at": chrono::Utc::now().to_rfc3339(),
            })))
            .send()
            .await
            .assert_status(202);
    }
    await_hwm(&harness, &id, topic, 4).await;

    let report = harness
        .backend()
        .maintain(
            harness.security_context(),
            &event_broker_sdk::RetentionRequest::for_partition(topic, 0, cutoff).build(),
        )
        .await
        .expect("a retention pass must run");
    assert_eq!(
        report.removed_events, 2,
        "the pass must take the two events assigned before the cutoff"
    );

    // 1 is now below the floor, and 2 - the erased event's own position - is
    // the floor itself.
    let below = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": topic, "partition": 0, "value": 1 }]
        })))
        .send()
        .await;
    below.assert_status(400);
    assert_eq!(
        below.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "Request validation failed",
            "instance": format!("/event-broker/v1/subscriptions/{id}:seek"),
            "context": {
                "field_violations": [{
                    "field": "partition_positions[0].value",
                    "description": "the position is below the valid range [2, 4]",
                    "reason": "below_retention_floor",
                }],
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );

    let onto_erased = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": topic, "partition": 0, "value": 2 }]
        })))
        .send()
        .await;
    onto_erased.assert_status(200);
    assert_eq!(
        onto_erased.json(),
        json!([{ "topic": topic, "partition": 0, "value": 2 }])
    );

    // And a stream opened on that cursor resumes at the next event still
    // stored, which is 3 - not at 2, and not at the start.
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

    let mut sequences: Vec<serde_json::Value> = Vec::new();
    for _ in 0..12 {
        let Ok(Some(Ok(chunk))) =
            tokio::time::timeout(std::time::Duration::from_secs(3), stream.next()).await
        else {
            break;
        };
        for part in String::from_utf8_lossy(&chunk).split("\r\n\r\n").skip(1) {
            let Ok(frame) =
                serde_json::from_str::<serde_json::Value>(part.trim_end_matches("\r\n"))
            else {
                continue;
            };
            if frame["kind"] == "event" {
                sequences.push(frame["payload"]["sequence"].clone());
            }
        }
        if sequences.len() == 2 {
            break;
        }
    }
    assert_eq!(
        sequences,
        vec![json!(3), json!(4)],
        "delivery resumes at the first stored event above the cursor"
    );
}

/// A partition retention has emptied admits only its ceiling.
///
/// There is no lower position that could be honoured, because no event remains
/// for delivery to resume from - so `earliest` and `latest` answer alike and
/// anything below is rejected. The ceiling is the highest sequence *assigned*,
/// which erasure never lowers.
#[tokio::test]
async fn an_emptied_partition_admits_only_its_ceiling() {
    let topic = "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1";
    let tenant_id = Uuid::new_v4();
    let harness = EventBrokerHarness::builder()
        .with_type_registry(StaticTypesRegistry::of(json!([
            { "id": topic, "partitions": 1 },
            {
                "id": "gts.cf.core.events.event.v1~x.eb.t1.foo.v1~",
                "topic": topic,
                "allowed_subject_types": ["gts.x.eb.t1.subject.v1~"],
            },
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
                "topic": topic,
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
    publish_and_settle(&harness, &id, topic, tenant_id, 3).await;

    let report = harness
        .backend()
        .maintain(
            harness.security_context(),
            &event_broker_sdk::RetentionRequest::for_partition(
                topic,
                0,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .build(),
        )
        .await
        .expect("a retention pass must run");
    assert_eq!(
        report.removed_events, 3,
        "the pass must empty the partition"
    );

    let ceiling = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": topic, "partition": 0, "value": 3 }]
        })))
        .send()
        .await;
    ceiling.assert_status(200);
    assert_eq!(
        ceiling.json(),
        json!([{ "topic": topic, "partition": 0, "value": 3 }])
    );

    // `earliest` answers the same, because there is no earlier position left.
    let earliest = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": topic, "partition": 0, "value": "earliest" }]
        })))
        .send()
        .await;
    earliest.assert_status(200);
    assert_eq!(
        earliest.json(),
        json!([{ "topic": topic, "partition": 0, "value": 3 }])
    );

    let below = harness
        .api_v1()
        .post_subscription_seek(&id)
        .with_body(Json(&json!({
            "partition_positions": [{ "topic": topic, "partition": 0, "value": 2 }]
        })))
        .send()
        .await;
    below.assert_status(400);
    assert_eq!(
        below.json(),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "Request validation failed",
            "instance": format!("/event-broker/v1/subscriptions/{id}:seek"),
            "context": {
                "field_violations": [{
                    "field": "partition_positions[0].value",
                    "description": "the position is below the valid range [3, 3]",
                    "reason": "below_retention_floor",
                }],
                "resource_type": "gts.cf.core.events.request.v1~",
            },
        })
    );
}

#[tokio::test]
async fn second_join_on_same_group_reduces_first_subscription_and_increments_version() {
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

    let resp_a = harness
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
        .await;
    resp_a.assert_status(201);
    let id_a = resp_a.json()["id"].as_str().unwrap().to_owned();
    assert_eq!(resp_a.json()["topology_version"], 1);
    assert_eq!(resp_a.json()["assigned"].as_array().unwrap().len(), 4);

    let resp_b = harness
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
        .await;
    resp_b.assert_status(201);
    assert_eq!(resp_b.json()["topology_version"], 2);
    assert_eq!(resp_b.json()["assigned"].as_array().unwrap().len(), 2);

    // First subscription's DB record must reflect the reduced assignment.
    let get_a = harness.api_v1().get_subscription(&id_a).send().await;
    get_a.assert_status(200);
    assert_eq!(get_a.json()["topology_version"], 2);
    assert_eq!(get_a.json()["assigned"].as_array().unwrap().len(), 2);
}

// -- client_agent encoding and length -------------------------------------

#[tokio::test]
async fn join_rejects_a_non_ascii_client_agent() {
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "agent\u{1}/1.0",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
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
            "detail": "Request validation failed",
            "instance": "/event-broker/v1/subscriptions",
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
async fn join_rejects_an_over_length_client_agent_before_parsing_session_timeout() {
    // `client_agent` is checked ahead of `session_timeout`, so a request that
    // breaks both reports the field violation rather than the duration parse.
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

    let resp = harness
        .api_v1()
        .post_subscriptions()
        .with_body(Json(&json!({
            "consumer_group": group,
            "client_agent": "a".repeat(257),
            "session_timeout": "garbage",
            "interests": [{
                "topic": "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1",
                "tenant_id": Uuid::new_v4(),
                "types": ["gts.cf.core.events.event.v1~x.eb.t1.foo.v1~"],
            }],
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
            "detail": "Request validation failed",
            "instance": "/event-broker/v1/subscriptions",
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
