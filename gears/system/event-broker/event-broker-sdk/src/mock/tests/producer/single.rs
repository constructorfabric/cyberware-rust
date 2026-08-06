//! Mirrors scenarios/producer/single/. Tests migrated per mock-reference-alignment.

use super::super::helpers::*;
#[cfg(test)]
use crate::sequence::Sequence;

use super::super::helpers::{broker_with_topic, ctx, wire_event};
use crate::api::{EventBrokerApi, IngestOutcome};
use crate::error::EventBrokerError;
use crate::mock::{MockBrokerHandle, PartitionKeyFixture, stubs::test_ctx_for_tenant};
use uuid::Uuid;

async fn populated_partition(handle: &MockBrokerHandle, topic: &str, partition_count: u32) -> u32 {
    let mut found = None;
    for partition in 0..partition_count {
        if !handle.stored(topic, partition).await.is_empty() {
            assert!(found.is_none(), "events span multiple partitions");
            found = Some(partition);
        }
    }
    found.expect("event was not stored")
}

/// Scenario: producer/single/1.01-positive-publish-single-async.md
#[tokio::test]
async fn s1_01_publish_single_async() {
    // Default publish path is async: durably enqueued → Accepted (202). No
    // offset/partition/sequence returned inline; they are server-stamped.
    let (broker, h) = broker_with_topic(TOPIC, 4).await;
    let c = ctx();
    let outcome = broker
        .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
        .await
        .unwrap();
    assert_eq!(outcome, IngestOutcome::Accepted);

    // Side effect: event lands on the tenant-derived partition and is stored.
    let p = populated_partition(&h, TOPIC, 4).await;
    assert_eq!(h.stored(TOPIC, p).await.len(), 1);
}

/// Scenario: producer/single/1.02-positive-publish-sync-wait-persisted.md
#[tokio::test]
async fn s1_02_publish_sync_wait_persisted() {
    // Sync-wait publish holds until the backend persists, returning Persisted
    // (201) instead of the default Accepted (202).
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    let outcome = broker
        .publish_sync(&c, &wire_event(EVT, c.subject_tenant_id()))
        .await
        .unwrap();
    assert_eq!(outcome, IngestOutcome::Persisted);
    // Persisted means the event is durably in the log before the call returns.
    assert_eq!(h.stored(TOPIC, 0).await.len(), 1);
}

/// Scenario: producer/single/1.03-negative-schema-validation-failure.md
#[tokio::test]
async fn s1_03_schema_validation_failure() {
    // data is validated against the event type's data_schema at ingest; a payload
    // failing validation is rejected (400 / EventDataInvalid), no event admitted.
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    h.register_event_type(
        TOPIC,
        EVT,
        serde_json::json!({
            "type": "object",
            "required": ["order_id", "total_cents"],
            "properties": {
                "order_id": { "type": "string" },
                "total_cents": { "type": "integer" }
            }
        }),
        &[],
    )
    .await;
    let c = ctx();
    let mut ev = wire_event(EVT, c.subject_tenant_id());
    // Missing the required `total_cents`.
    ev.data = Some(serde_json::json!({ "order_id": "order-bad" }));
    let err = broker.publish(&c, &ev).await.unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("EventDataInvalid") || msg.contains("validation") || msg.contains("invalid"),
        "bad payload must fail validation: {msg}"
    );
    // No event admitted.
    assert!(h.stored(TOPIC, 0).await.is_empty());
}

#[tokio::test]
async fn registered_event_type_allowed_subjects_are_enforced() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    h.register_event_type(
        TOPIC,
        EVT,
        serde_json::json!({
            "type": "object"
        }),
        &["test-type"],
    )
    .await;
    let c = ctx();
    let accepted = wire_event(EVT, c.subject_tenant_id());
    assert_eq!(
        broker.publish(&c, &accepted).await.unwrap(),
        IngestOutcome::Accepted
    );

    let mut rejected = wire_event(EVT, c.subject_tenant_id());
    rejected.subject_type = "other-type".to_owned();
    let err = broker.publish(&c, &rejected).await.unwrap_err();
    assert!(
        matches!(err, EventBrokerError::InvalidEventField { field, .. } if field == "subject_type"),
        "disallowed subject_type must be rejected: {err:?}"
    );
    assert_eq!(
        h.stored(TOPIC, 0).await.len(),
        1,
        "rejected subject_type must not be admitted"
    );
}

/// Scenario: producer/single/1.04-negative-rate-limited.md
#[tokio::test]
async fn s1_04_rate_limited() {
    // When the tenant publish quota is exhausted, the next publish is refused
    // (429 / RateLimited) and no event is admitted.
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let handle = broker.handle();
    // Some(0) → zero allowance: the next publish (which charges 1 unit) is refused.
    handle.set_publish_rate_limit(Some(0)).await;
    let c = ctx();
    let err = broker
        .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
        .await
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("RateLimited") || msg.contains("rate limit"),
        "exhausted quota must yield RateLimited: {msg}"
    );
    // No event admitted.
    assert!(h.stored(TOPIC, 0).await.is_empty());
}

/// Scenario: producer/single/1.05-negative-readonly-partition-rejected.md
#[tokio::test]
async fn s1_05_readonly_partition_rejected() {
    let (broker, h) = broker_with_topic(TOPIC, 4).await;
    let c = ctx();
    let mut ev = wire_event(EVT, c.subject_tenant_id());
    ev.partition = Some(3);

    let err = broker.publish(&c, &ev).await.unwrap_err();
    assert!(
        matches!(err, EventBrokerError::InvalidEventField { field, .. } if field == "partition"),
        "producer-supplied partition must be rejected as read-only: {err:?}"
    );
    for partition in 0..4 {
        assert!(
            h.stored(TOPIC, partition).await.is_empty(),
            "rejected event must not be admitted to partition {partition}"
        );
    }
}

/// Scenario: producer/single/1.01-positive-publish-single-async.md
#[tokio::test]
async fn s1_01_the_default_pointer_routes_by_tenant() {
    // The type declares no pointer, so the base's default applies: routes by
    // tenant per ADR-0002.
    let (broker, h) = broker_with_topic(TOPIC, 8).await;
    let t1 = Uuid::parse_str("aaaaaaaa-0000-0000-0000-000000000001").unwrap();
    let c1 = test_ctx_for_tenant(t1);
    let mut first = wire_event(EVT, c1.subject_tenant_id());
    first.subject = "first-subject".to_owned();
    let mut second = wire_event(EVT, c1.subject_tenant_id());
    second.subject = "different-subject".to_owned();

    broker.publish(&c1, &first).await.unwrap();
    broker.publish(&c1, &second).await.unwrap();

    let partition = populated_partition(&h, TOPIC, 8).await;
    let stored = h.stored(TOPIC, partition).await;
    assert_eq!(stored.len(), 2, "same tenant must select one partition");
    assert!(
        stored
            .iter()
            .all(|event| event.event.partition == Some(partition))
    );
}

/// Scenario: producer/single/1.01-positive-publish-single-async.md
#[tokio::test]
async fn s1_01_a_declared_pointer_selects_the_partition_input() {
    // The type points at `/subject` rather than the default tenant, so two events
    // from *different* tenants sharing a subject share a partition - which is the
    // point: the key is the type's choice, not the publisher's.
    let (broker, h) = broker_with_topic(TOPIC2, 4).await;
    h.set_partition_key(PartitionKeyFixture {
        event_type: EVT,
        pointer: "/subject",
    })
    .await;
    let c = ctx();
    let mut ev = wire_event(EVT, c.subject_tenant_id());
    ev.subject = "explicit-key".to_owned();
    let other =
        test_ctx_for_tenant(Uuid::parse_str("bbbbbbbb-0000-0000-0000-000000000001").unwrap());
    let mut other_ev = wire_event(EVT, other.subject_tenant_id());
    other_ev.subject = "explicit-key".to_owned();

    broker.publish(&c, &ev).await.unwrap();
    broker.publish(&other, &other_ev).await.unwrap();

    let partition = populated_partition(&h, TOPIC2, 4).await;
    let stored = h.stored(TOPIC2, partition).await;
    assert_eq!(
        stored.len(),
        2,
        "one pointed-at value must select one partition"
    );
    assert!(
        stored
            .iter()
            .all(|event| event.event.partition == Some(partition))
    );
    assert!(
        stored
            .iter()
            .all(|event| event.event.subject == "explicit-key")
    );
}

/// Scenario: producer/single/1.01-positive-publish-single-async.md
#[tokio::test]
async fn s1_01_same_tenant_events_same_partition() {
    let (broker, h) = broker_with_topic(TOPIC3, 8).await;
    let c = ctx();
    for _ in 0..5 {
        broker
            .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
            .await
            .unwrap();
    }
    let expected_p = populated_partition(&h, TOPIC3, 8).await;
    assert_eq!(h.stored(TOPIC3, expected_p).await.len(), 5);
}

/// Scenario: producer/single/1.01-positive-publish-single-async.md
#[tokio::test]
async fn s1_01_offsets_are_monotonic_per_partition() {
    // Offsets are server-assigned; per partition they are monotonic from 0.
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    for _ in 0..5 {
        broker
            .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
            .await
            .unwrap();
    }
    let stored = h.stored(TOPIC, 0).await;
    assert_eq!(stored.len(), 5);
    for (i, se) in stored.iter().enumerate() {
        // Sequences are 1-based (A6/A7): the i-th stored event has sequence i+1.
        assert_eq!(se.event.sequence.unwrap(), Sequence::assigned((i as i64) + 1));
    }
}
