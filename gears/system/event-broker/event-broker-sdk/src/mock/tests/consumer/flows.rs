//! Mirrors scenarios/consumer/flows/. Tests migrated per mock-reference-alignment.

use super::super::helpers::*;
#[cfg(test)]
use crate::sequence::Sequence;

use super::super::helpers::{broker_with_topic, ctx, ctx2, join_group, make_group, wire_event};
use crate::Position;
use crate::api::EventBrokerApi;
use crate::api::{ControlCode, SeekPosition, WireFrame};
use futures_util::StreamExt;
use std::time::Duration;

/// Read the next stream frame within a bounded timeout (live streams never end).
async fn next_frame(
    s: &mut crate::api::FrameStream,
) -> Option<Result<WireFrame, crate::error::EventBrokerError>> {
    tokio::time::timeout(Duration::from_millis(200), s.next())
        .await
        .unwrap_or_default()
}

/// Scenario: consumer/flows/1.01-flow-two-consumer-rebalance.md
///
/// A group grows from one consumer to two: A owns all 4 partitions and streams; B
/// JOINs, triggering a rebalance to 2/2; A's open stream receives a non-terminal
/// `Topology` frame (A loses 2 partitions, keeps streaming); B SEEKs its gained
/// partitions and streams. Strictly broker-observable.
#[tokio::test]
async fn s1_01_flow_two_consumer_rebalance() {
    let (broker, h) = broker_with_topic(TOPIC, 4).await;
    let c = ctx();
    let c2 = ctx2();
    let gid = make_group(&c, &broker).await;

    // Exchange 1 - A JOINs (owns all 4).
    let sub_a = join_group(&c, &broker, &gid, TOPIC).await;
    assert_eq!(sub_a.topology_version, 1);
    assert_eq!(
        h.assignment(sub_a.subscription_id).await.len(),
        4,
        "A owns all 4 partitions"
    );

    // Exchange 2 - A SEEKs all four, then streams.
    broker
        .seek(
            &c,
            sub_a.subscription_id,
            &(0u32..4)
                .map(|p| SeekPosition {
                    topic: TOPIC.to_owned(),
                    partition: p,
                    value: Position::Earliest,
                })
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    let mut s_a = broker.stream(&c, sub_a.subscription_id).await.unwrap();
    let _ = next_frame(&mut s_a).await; // open-time baseline (4 partitions)

    // Exchange 3 - B JOINs (triggers rebalance to 2/2).
    let sub_b = join_group(&c2, &broker, &gid, TOPIC).await;
    assert_eq!(sub_b.topology_version, 2, "topology_version advances 1 → 2");
    assert_eq!(
        h.assignment(sub_b.subscription_id).await.len(),
        2,
        "B gains 2 partitions"
    );
    assert_eq!(
        h.assignment(sub_a.subscription_id).await.len(),
        2,
        "A keeps 2 partitions"
    );

    // Exchange 4 - A's open stream receives a non-terminal topology frame.
    let mut reduced = None;
    for _ in 0..50 {
        match next_frame(&mut s_a).await {
            Some(Ok(WireFrame::Topology {
                topology_version,
                assigned,
            })) => {
                assert_eq!(topology_version, 2);
                reduced = Some(assigned);
                break;
            }
            Some(Ok(WireFrame::Control { .. })) => panic!("a loss must not terminate A"),
            Some(Ok(_)) => {}
            _ => {}
        }
    }
    assert_eq!(
        reduced.expect("A receives a topology frame").len(),
        2,
        "A reduced to 2 partitions"
    );

    // Exchange 5/6 - B SEEKs its gained partitions and opens its stream.
    let b_slots = h.assignment(sub_b.subscription_id).await;
    broker
        .seek(
            &c2,
            sub_b.subscription_id,
            &b_slots
                .iter()
                .map(|sl| SeekPosition {
                    topic: sl.topic.clone(),
                    partition: sl.partition,
                    value: Position::Earliest,
                })
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    assert!(
        broker.stream(&c2, sub_b.subscription_id).await.is_ok(),
        "B opens its stream after seeding"
    );

    // No partition delivered to both A and B.
    let a_set: std::collections::HashSet<u32> = h
        .assignment(sub_a.subscription_id)
        .await
        .iter()
        .map(|s| s.partition)
        .collect();
    let b_set: std::collections::HashSet<u32> = b_slots.iter().map(|s| s.partition).collect();
    assert!(
        a_set.is_disjoint(&b_set),
        "no partition is owned by both A and B"
    );
}

/// Scenario: consumer/flows/1.02-flow-positions-not-set-recovery.md
///
/// DIVERGENCE (PositionsNotSet 409 is unrepresentable in the mock): the scenario's
/// recovery transcript is open-without-seed → `409 PositionsNotSet` → SEEK the listed
/// partitions → retry stream → `200`. The mock has no unseeded-stream precondition, so
/// the rejecting first exchange cannot occur (the stream opens unconditionally). The
/// representable half is exercised: SEEK seeds the partition, then the stream delivers
/// from the seeded floor. The 409-then-retry control flow is HTTP/SDK-layer.
#[tokio::test]
async fn s1_02_flow_positions_not_set_recovery() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    broker
        .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
        .await
        .unwrap();
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    // Exchange 1 (divergence): opening before seeding does NOT 409 in the mock - it
    // opens. We instead assert the recovery half directly.
    assert_eq!(
        h.cursor(&gid, TOPIC, 0).await,
        None,
        "no committed cursor before SEEK"
    );

    // Exchange 2 - SEEK the unseeded partition.
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
    assert_eq!(results[0].offset, Sequence::assigned(0));
    assert_eq!(
        h.cursor(&gid, TOPIC, 0).await,
        Some(Sequence::assigned(0)),
        "SEEK seeds the cursor"
    );

    // Exchange 3 - the (re)opened stream delivers from the seeded floor.
    let mut s = broker.stream(&c, sub.subscription_id).await.unwrap();
    let _ = next_frame(&mut s).await; // baseline
    let mut got_event = false;
    for _ in 0..10 {
        match next_frame(&mut s).await {
            Some(Ok(WireFrame::Event(e))) => {
                assert_eq!(
                    e.sequence,
                    Sequence::assigned(1),
                    "emission begins at the seeded floor (1-based; Earliest→cursor 0, emit from 1)"
                );
                got_event = true;
                break;
            }
            Some(Ok(_)) => {}
            _ => break,
        }
    }
    assert!(got_event, "after seeding, the stream delivers the event");
}

/// Scenario: consumer/flows/1.03-flow-path-a-consumer-with-db.md
///
/// A consumer with its own offset DB: on (re)start it JOINs, SEEKs from its stored
/// last-processed offsets, and streams. On reconnect after a crash it resumes from
/// the persisted offset with no reprocessing. (Exchanges 1/5 are consumer-internal.)
///
/// DIVERGENCE (emit-from-offset semantics): the scenario treats the SEEK integer as
/// the *last-processed* offset and emits from `offset + 1`. The mock emits from the
/// SEEK offset *inclusive* (`start = seek_offset`), so a SEEK(510) delivers offset 510
/// first. The broker-observable JOIN → SEEK(exact) → stream → re-JOIN → SEEK(exact)
/// resume-without-reprocessing path is asserted with the mock's inclusive semantics.
#[tokio::test]
async fn s1_03_flow_path_a_consumer_with_db() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    // 600 events so exact offsets like 510 are valid.
    for _ in 0..600 {
        broker
            .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
            .await
            .unwrap();
    }
    let gid = make_group(&c, &broker).await;

    // Exchange 2 - JOIN.
    let sub = join_group(&c, &broker, &gid, TOPIC).await;

    // Exchange 3 - SEEK from the consumer's own DB (last-processed offset 510).
    let results = broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::Exact(Sequence::assigned(510)),
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        results[0].offset,
        Sequence::assigned(510),
        "exact stored offset is echoed verbatim"
    );
    assert_eq!(
        h.cursor(&gid, TOPIC, 0).await,
        Some(Sequence::assigned(510))
    );

    // Exchange 4 - stream delivers from the seeked offset (mock: inclusive → 510).
    let mut s = broker.stream(&c, sub.subscription_id).await.unwrap();
    let _ = next_frame(&mut s).await; // baseline
    let mut first_event_sequence = None;
    for _ in 0..10 {
        match next_frame(&mut s).await {
            Some(Ok(WireFrame::Event(e))) => {
                first_event_sequence = Some(e.sequence);
                break;
            }
            Some(Ok(_)) => {}
            _ => break,
        }
    }
    assert_eq!(
        first_event_sequence,
        Some(Sequence::assigned(511)),
        "emit from cursor+1: SEEK 510 (last-processed) → first delivered is 511"
    );
    drop(s);

    // Exchange 6 - reconnect after crash: re-JOIN, SEEK with the next unprocessed
    // offset (511), resume from 511 (mock inclusive). No reprocessing of 510.
    broker.leave(&c, sub.subscription_id).await.unwrap();
    let sub2 = join_group(&c, &broker, &gid, TOPIC).await;
    broker
        .seek(
            &c,
            sub2.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition: 0,
                value: Position::Exact(Sequence::assigned(511)),
            }],
        )
        .await
        .unwrap();
    let mut s2 = broker.stream(&c, sub2.subscription_id).await.unwrap();
    let _ = next_frame(&mut s2).await; // baseline
    let mut resume_sequence = None;
    for _ in 0..10 {
        match next_frame(&mut s2).await {
            Some(Ok(WireFrame::Event(e))) => {
                resume_sequence = Some(e.sequence);
                break;
            }
            Some(Ok(_)) => {}
            _ => break,
        }
    }
    assert_eq!(
        resume_sequence,
        Some(Sequence::assigned(512)),
        "after reconnect, resumes from cursor+1 (SEEK 511 → first delivered 512)"
    );
}

/// Scenario: consumer/flows/1.04-flow-leave-triggers-gain-terminate.md
///
/// The gain → terminate transcript: A=[0,1], B=[2,3], both streaming. B LEAVEs; the
/// rebalance gives A all 4 (A GAINS). Because a gained partition is unseeded, A's
/// subscription terminates: its stream emits a `Control{code: Terminal}` carrying the
/// final positions, then closes. A re-JOINs (new id), SEEKs, reopens. Reuse of the old
/// (terminated) id is rejected (410-equivalent).
#[tokio::test]
async fn s1_04_flow_leave_triggers_gain_terminate() {
    let (broker, h) = broker_with_topic(TOPIC, 4).await;
    let c = ctx();
    let c2 = ctx2();
    let gid = make_group(&c, &broker).await;

    // A and B split 2/2; A is streaming.
    let sub_a = join_group(&c, &broker, &gid, TOPIC).await;
    // Seed while A still owns all 4 (before B joins); the kept partitions stay seeded.
    super::super::helpers::seek_all_earliest(&c, &broker, &sub_a).await;
    let sub_b = join_group(&c2, &broker, &gid, TOPIC).await;
    assert_eq!(h.assignment(sub_a.subscription_id).await.len(), 2);
    assert_eq!(h.assignment(sub_b.subscription_id).await.len(), 2);

    let mut s_a = broker.stream(&c, sub_a.subscription_id).await.unwrap();
    let _ = next_frame(&mut s_a).await; // open-time baseline (2 partitions)

    // Exchange 1 - B LEAVEs → rebalance gives A all 4 (A gains).
    broker.leave(&c2, sub_b.subscription_id).await.unwrap();

    // Exchange 2 - A's stream emits a terminal control frame, then closes.
    let mut positions = None;
    let mut saw_event_or_hb = Vec::new();
    for _ in 0..50 {
        match next_frame(&mut s_a).await {
            Some(Ok(WireFrame::Control {
                code: ControlCode::Terminal,
                positions: p,
                reason,
            })) => {
                assert_eq!(
                    reason.as_deref(),
                    Some("rebalanced"),
                    "terminal reason is 'rebalanced'"
                );
                positions = Some(p);
                break;
            }
            Some(Ok(WireFrame::Event(_))) => saw_event_or_hb.push("event"),
            Some(Ok(WireFrame::Heartbeat { .. })) => saw_event_or_hb.push("heartbeat"),
            Some(Ok(WireFrame::Topology { .. })) => saw_event_or_hb.push("topology"),
            Some(Ok(WireFrame::Control { .. })) => saw_event_or_hb.push("control(other)"),
            Some(Err(e)) => panic!("stream error before terminal: {e:?}; saw {saw_event_or_hb:?}"),
            None => break,
        }
    }
    let positions =
        positions.unwrap_or_else(|| panic!("no terminal control frame; saw {saw_event_or_hb:?}"));
    assert!(
        !positions.is_empty(),
        "terminal frame carries the final positions"
    );
    assert!(
        next_frame(&mut s_a).await.is_none(),
        "stream closes after the terminal frame"
    );

    // Exchange 3 - A re-JOINs (new subscription, owns all 4).
    let sub_a2 = join_group(&c, &broker, &gid, TOPIC).await;
    assert_ne!(
        sub_a2.subscription_id, sub_a.subscription_id,
        "a new subscription id is issued"
    );
    assert_eq!(
        h.assignment(sub_a2.subscription_id).await.len(),
        4,
        "the re-JOIN owns all 4"
    );

    // Exchange 4 - A SEEKs all four and reopens.
    broker
        .seek(
            &c,
            sub_a2.subscription_id,
            &(0u32..4)
                .map(|p| SeekPosition {
                    topic: TOPIC.to_owned(),
                    partition: p,
                    value: Position::Earliest,
                })
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    assert!(
        broker.stream(&c, sub_a2.subscription_id).await.is_ok(),
        "A reopens on the new id"
    );

    // Exchange 5 - reuse of the old (terminated) id is rejected (410-equivalent).
    assert!(
        broker.stream(&c, sub_a.subscription_id).await.is_err(),
        "reusing the terminated subscription id is rejected (410)"
    );
}
