//! Mirrors scenarios/consumer/positions/. Tests migrated per mock-reference-alignment.

use super::super::helpers::*;
#[cfg(test)]
use crate::sequence::Sequence;

use super::super::helpers::{
    broker_with_topic, ctx, join_group, make_group, register_topic_with_event_type, wire_event,
};
use crate::Position;
use crate::api::EventBrokerApi;
use crate::api::SeekPosition;
use crate::models::Event;
use serde_json::json;
use uuid::Uuid;

/// Build a wire event with an explicit `occurred_at` so timestamp-seek scenarios
/// can place events on the partition's time axis deterministically.
fn wire_event_at(type_id: &str, tenant_id: Uuid, occurred_at: &str) -> Event {
    let mut ev = wire_event(type_id, tenant_id);
    ev.occurred_at = chrono::DateTime::parse_from_rfc3339(occurred_at)
        .expect("wire_event_at: occurred_at must be RFC3339")
        .with_timezone(&chrono::Utc);
    ev
}

/// Scenario: consumer/positions/1.01-positive-seek-earliest.md
///
/// SEEK with the `Earliest` sentinel resolves server-side to an integer cursor.
/// (Divergence: the scenario resolves `earliest` → `RF - 1`; the mock has no
/// retention floor, so `Earliest` resolves to `0` - the floor of an unpruned log.
/// The asserted contract is "sentinel resolved server-side to an integer cursor".)
#[tokio::test]
async fn s1_01_positive_seek_earliest() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    for _ in 0..3 {
        broker
            .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
            .await
            .unwrap();
    }
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    let results = broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::Earliest,
            }],
        )
        .await
        .unwrap();

    assert_eq!(
        results.len(),
        1,
        "one resolved position per requested partition"
    );
    assert_eq!(results[0].topic, TOPIC);
    assert_eq!(results[0].partition, 0);
    assert_eq!(
        results[0].offset,
        Sequence::assigned(0),
        "Earliest resolves to the floor cursor (0)"
    );
    assert_eq!(
        h.cursor(&gid, TOPIC, 0).await,
        Some(Sequence::assigned(0)),
        "group cursor is seeded to the resolved earliest offset"
    );
}

/// Scenario: consumer/positions/1.02-positive-seek-latest.md
///
/// SEEK with the `Latest` sentinel resolves to the current high-water mark, so
/// only events admitted after the SEEK are delivered. Mock HWM = `next_offset - 1`.
#[tokio::test]
async fn s1_02_positive_seek_latest() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    // Five events → offsets 0..4, next_offset = 5, HWM = 4.
    for _ in 0..5 {
        broker
            .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
            .await
            .unwrap();
    }
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    let results = broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::Latest,
            }],
        )
        .await
        .unwrap();

    assert_eq!(
        results[0].offset,
        Sequence::assigned(5),
        "Latest resolves to the current HWM (1-based: 5 events → offset 5)"
    );
    assert_eq!(
        h.cursor(&gid, TOPIC, 0).await,
        Some(Sequence::assigned(5)),
        "group cursor is seeded to the HWM"
    );
}

/// Scenario: consumer/positions/1.03-positive-seek-exact-offset.md
///
/// An explicit integer SEEK value is stored verbatim as the last-processed offset.
#[tokio::test]
async fn s1_03_positive_seek_exact_offset() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    for _ in 0..100 {
        broker
            .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
            .await
            .unwrap();
    }
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    let results = broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::Exact(Sequence::assigned(42)),
            }],
        )
        .await
        .unwrap();

    assert_eq!(
        results[0].offset,
        Sequence::assigned(42),
        "exact offset is stored verbatim (no +1 on the wire)"
    );
    assert_eq!(h.cursor(&gid, TOPIC, 0).await, Some(Sequence::assigned(42)));
}

/// Scenario: consumer/positions/1.04-positive-mixed-sentinels.md
///
/// One SEEK request carries a different value kind per partition; each is resolved
/// independently. (Divergence: the scenario spans three partitions on one topic
/// with distinct RF/HWM; tenant-hash partitioning in the mock routes all events to
/// a single partition, so the per-partition independence is exercised across three
/// single-partition topics instead - same resolution contract.)
#[tokio::test]
async fn s1_04_positive_mixed_sentinels() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    h.register_topic(TOPIC2, 1).await;
    // TOPIC3 is published to, so it needs its own event type: the topic an event
    // lands on is resolved from its type, and EVT already belongs to TOPIC.
    register_topic_with_event_type(&h, TOPIC3, 1, EVT3).await;
    let c = ctx();
    // TOPIC: events so an exact offset is valid. TOPIC3: 5 events → HWM 4.
    for _ in 0..100 {
        broker
            .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
            .await
            .unwrap();
    }
    for _ in 0..5 {
        broker
            .publish(&c, &wire_event(EVT3, c.subject_tenant_id()))
            .await
            .unwrap();
    }
    let gid = make_group(&c, &broker).await;
    let sub = broker
        .join(
            &c,
            crate::api::JoinRequest {
                group: gid,
                client_agent: "mixed-consumer/1.0".into(),
                interests: vec![interest(TOPIC), interest(TOPIC2), interest(TOPIC3)],
                session_timeout: None,
            },
        )
        .await
        .unwrap();

    let results = broker
        .seek(
            &c,
            sub.subscription_id,
            &[
                SeekPosition {
                    topic: TOPIC.to_owned(),
                    partition: 0,
                    value: Position::Exact(Sequence::assigned(42)),
                },
                SeekPosition {
                    topic: TOPIC2.to_owned(),
                    partition: 0,
                    value: Position::Earliest,
                },
                SeekPosition {
                    topic: TOPIC3.to_owned(),
                    partition: 0,
                    value: Position::Latest,
                },
            ],
        )
        .await
        .unwrap();

    let by_topic = |t: &str| results.iter().find(|r| r.topic == t).unwrap().offset;
    assert_eq!(by_topic(TOPIC), Sequence::assigned(42), "exact verbatim");
    assert_eq!(
        by_topic(TOPIC2),
        Sequence::NONE,
        "Earliest → floor cursor (RF−1 = 0)"
    );
    assert_eq!(
        by_topic(TOPIC3),
        Sequence::assigned(5),
        "Latest → HWM (1-based: 5 events → offset 5)"
    );
    assert_eq!(h.cursor(&gid, TOPIC, 0).await, Some(Sequence::assigned(42)));
    assert_eq!(h.cursor(&gid, TOPIC2, 0).await, Some(Sequence::assigned(0)));
    assert_eq!(h.cursor(&gid, TOPIC3, 0).await, Some(Sequence::assigned(5)));
}

/// Scenario: consumer/positions/1.05-negative-out-of-range-offset.md
#[tokio::test]
async fn s1_05_negative_out_of_range_offset() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    // Nothing published, so the partition has assigned nothing: the only
    // position it admits is `0`, and 5 is above its ceiling.
    let err = broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::Exact(Sequence::assigned(5)),
            }],
        )
        .await
        .expect_err("an out-of-range seek must be rejected");
    assert_eq!(
        problem_json(err),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "Request validation failed",
            "instance": "/v1/subscriptions/test:seek",
            "trace_id": "trace-123",
            "context": {
                "field_violations": [{
                    "field": "partition_positions[0].value",
                    "description": "the position is above the valid range [0, 0]",
                    "reason": "above_high_water_mark"
                }],
                "resource_type": crate::error::resources::REQUEST
            }
        })
    );
    assert_eq!(
        h.cursor(&gid, TOPIC, 0).await,
        None,
        "no cursor committed on a rejected seek"
    );
}

/// Scenario: consumer/positions/1.06-negative-offset-above-hwm.md
#[tokio::test]
async fn s1_06_negative_offset_above_hwm() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    for _ in 0..3 {
        broker
            .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
            .await
            .unwrap();
    }
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    // Three events assigned, so the ceiling is 3 and nothing has been erased,
    // putting the floor at 0. 100_000 is far above the ceiling.
    let err = broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::Exact(Sequence::assigned(100_000)),
            }],
        )
        .await
        .expect_err("a seek above the HWM must be rejected");
    assert_eq!(
        problem_json(err),
        json!({
            "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~",
            "title": "Invalid Argument",
            "status": 400,
            "detail": "Request validation failed",
            "instance": "/v1/subscriptions/test:seek",
            "trace_id": "trace-123",
            "context": {
                "field_violations": [{
                    "field": "partition_positions[0].value",
                    "description": "the position is above the valid range [0, 3]",
                    "reason": "above_high_water_mark"
                }],
                "resource_type": crate::error::resources::REQUEST
            }
        })
    );
    assert_eq!(h.cursor(&gid, TOPIC, 0).await, None);
}

/// Scenario: consumer/positions/1.07-negative-seek-while-streaming.md
///
/// SEEK is a pre-stream operation. A SEEK issued while a stream is open is rejected
/// with `StreamingInProgress` and leaves the cursor unchanged.
#[tokio::test]
async fn s1_07_negative_seek_while_streaming() {
    use futures_util::StreamExt;
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    // Seed the cursor (Earliest → 0; in range on an empty partition).
    broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::Earliest,
            }],
        )
        .await
        .unwrap();
    assert_eq!(h.cursor(&gid, TOPIC, 0).await, Some(Sequence::assigned(0)));

    // Open a stream (drain the open-time topology baseline).
    let mut stream = broker.stream(&c, sub.subscription_id).await.unwrap();
    let _ = stream.next().await;

    // SEEK is a pre-stream operation: with a stream open it is rejected.
    let err = broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::Exact(Sequence::assigned(400)),
            }],
        )
        .await
        .expect_err("SEEK while streaming must be rejected");
    assert!(
        matches!(
            err,
            crate::error::EventBrokerError::StreamingInProgress { .. }
        ),
        "expected StreamingInProgress, got {err:?}"
    );
    // Cursor is unchanged by the rejected seek.
    assert_eq!(h.cursor(&gid, TOPIC, 0).await, Some(Sequence::assigned(0)));
}

/// Scenario: consumer/positions/1.09-negative-seek-unassigned-partition.md
///
/// DIVERGENCE (PartitionNotAssigned is unrepresentable in the mock): the scenario
/// expects `409 PartitionNotAssigned` (atomic - nothing applied) when a SEEK names a
/// partition not assigned to the subscription. The mock's `seek` does not validate
/// the requested `(topic, partition)` against the subscription's assignment - it
/// resolves and commits to the GROUP cursor regardless. Closest real assertion: a
/// SEEK on a partition outside the assignment still succeeds and writes the group
/// cursor (the 409 assignment guard is HTTP-only).
#[tokio::test]
async fn s1_09_negative_seek_unassigned_partition() {
    let (broker, h) = broker_with_topic(TOPIC, 4).await;
    let c = ctx();
    let c2 = ctx2();
    let gid = make_group(&c, &broker).await;
    // Two members split 4 partitions 2/2 so each owns only a subset.
    let sub_a = join_group(&c, &broker, &gid, TOPIC).await;
    let _sub_b = join_group(&c2, &broker, &gid, TOPIC).await;

    let a_slots = h.assignment(sub_a.subscription_id).await;
    assert_eq!(a_slots.len(), 2, "A owns 2 of 4 partitions");
    // Pick a partition A does NOT own.
    let owned: std::collections::HashSet<u32> = a_slots.iter().map(|s| s.partition).collect();
    let unassigned = (0u32..4).find(|p| !owned.contains(p)).unwrap();

    let err = broker
        .seek(
            &c,
            sub_a.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: unassigned,
                value: Position::Earliest,
            }],
        )
        .await
        .expect_err("seek to an unassigned partition must be rejected");
    assert!(
        matches!(err, crate::error::EventBrokerError::PartitionNotAssigned { partition, .. } if partition == unassigned),
        "expected PartitionNotAssigned for {unassigned}, got {err:?}"
    );
    // No cursor is committed for the unassigned partition.
    assert_eq!(h.cursor(&gid, TOPIC, unassigned).await, None);
}

/// Scenario: consumer/positions/1.10-positive-seek-any-value-in-range.md
///
/// Pre-stream SEEK is not subject to a forward-only rule against the SESSION cursor;
/// a fresh subscription may seed any value. (Divergence: the mock's group cursor is
/// forward-only MAX, so a *lower* group cursor cannot be set by a later SEEK from a
/// continuing group; this test seeds a fresh group, where any value is accepted, and
/// asserts the resolved value is committed verbatim.)
#[tokio::test]
async fn s1_10_positive_seek_any_value_in_range() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    for _ in 0..600 {
        broker
            .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
            .await
            .unwrap();
    }
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    // Fresh subscription, no prior cursor: seed 100 directly.
    let results = broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::Exact(Sequence::assigned(100)),
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        results[0].offset,
        Sequence::assigned(100),
        "pre-stream SEEK accepts any in-range value"
    );
    assert_eq!(
        h.cursor(&gid, TOPIC, 0).await,
        Some(Sequence::assigned(100))
    );
}

/// Scenario: consumer/positions/1.11-positive-seek-at-timestamp.md
///
/// `AtTimestamp` resolves to the offset of the first event whose `occurred_at` is at
/// or after the timestamp (linear `occurred_at` scan). With one event strictly before
/// the timestamp and one at/after it, the resolved cursor is the at/after event's
/// offset. (Divergence: the scenario partitions 0..3 each resolve independently to
/// different offsets; the mock routes a tenant's events to a single partition, so the
/// at-timestamp resolution is exercised on partition 0 only - same scan contract.)
#[tokio::test]
async fn s1_11_positive_seek_at_timestamp() {
    let (broker, _h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    let t = c.subject_tenant_id();
    // offset 0 @ 09:59:50 (before), offset 1 @ 10:00:03 (at/after target 10:00:00).
    broker
        .publish(&c, &wire_event_at(EVT, t, "2026-06-14T09:59:50Z"))
        .await
        .unwrap();
    broker
        .publish(&c, &wire_event_at(EVT, t, "2026-06-14T10:00:03Z"))
        .await
        .unwrap();

    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    let results = broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::At(
                    "2026-06-14T10:00:00Z"
                        .parse()
                        .expect("a fixed RFC 3339 instant"),
                ),
            }],
        )
        .await
        .unwrap();

    assert_eq!(
        results[0].offset,
        Sequence::assigned(1),
        "resolves to the first event at/after the timestamp (offset 1)"
    );
}

/// Scenario: consumer/positions/1.12-positive-seek-at-timestamp-before-retention.md
///
/// A timestamp at/before the oldest stored event resolves to the retention floor
/// (the oldest stored offset) - behaviour identical to `Earliest`.
#[tokio::test]
async fn s1_12_positive_seek_at_timestamp_before_retention() {
    let (broker, _h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    let t = c.subject_tenant_id();
    // Oldest stored event is at 2026-01-01; the requested ts (2025) predates it.
    broker
        .publish(&c, &wire_event_at(EVT, t, "2026-01-01T00:00:00Z"))
        .await
        .unwrap();
    broker
        .publish(&c, &wire_event_at(EVT, t, "2026-02-01T00:00:00Z"))
        .await
        .unwrap();

    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    let results = broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::At(
                    "2025-06-01T00:00:00Z"
                        .parse()
                        .expect("a fixed RFC 3339 instant"),
                ),
            }],
        )
        .await
        .unwrap();

    assert_eq!(
        results[0].offset,
        Sequence::assigned(0),
        "ts before the floor clamps to the oldest stored offset (the floor)"
    );
}

/// Scenario: consumer/positions/1.13-positive-seek-at-timestamp-beyond-hwm.md
///
/// A timestamp beyond the newest stored event resolves to the high-water mark -
/// behaviour identical to `Latest`.
#[tokio::test]
async fn s1_13_positive_seek_at_timestamp_beyond_hwm() {
    let (broker, _h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    let t = c.subject_tenant_id();
    // Two events; newest at 2026-06; HWM = offset 1. Requested ts (2030) is beyond.
    broker
        .publish(&c, &wire_event_at(EVT, t, "2026-06-01T00:00:00Z"))
        .await
        .unwrap();
    broker
        .publish(&c, &wire_event_at(EVT, t, "2026-06-02T00:00:00Z"))
        .await
        .unwrap();

    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    let results = broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::At(
                    "2030-01-01T00:00:00Z"
                        .parse()
                        .expect("a fixed RFC 3339 instant"),
                ),
            }],
        )
        .await
        .unwrap();

    assert_eq!(
        results[0].offset,
        Sequence::assigned(2),
        "ts beyond the newest event resolves to the HWM (2 events → HWM offset 2)"
    );
}

/// JOIN interest helper mirroring `helpers::join_group`'s single interest.
fn interest(topic: &str) -> crate::api::SubscriptionInterest {
    crate::api::SubscriptionInterest {
        topic: topic.to_owned(),
        tenant_id: uuid::Uuid::nil(),
        tenant_depth: crate::api::TenantTraversalDepth::CurrentTenant,
        barrier_mode: crate::api::BarrierMode::Respect,
        types: vec!["*".to_owned()],
        filter: None,
    }
}
