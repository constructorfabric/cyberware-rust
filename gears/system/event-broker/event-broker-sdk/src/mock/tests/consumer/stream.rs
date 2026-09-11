//! Mirrors scenarios/consumer/stream/. Tests migrated per mock-reference-alignment.

use super::super::helpers::*;
use crate::sequence::Sequence;
#[cfg(test)]
use toolkit_gts::gts_id;

use super::super::helpers::{broker_with_topic, ctx, ctx2, join_group, make_group, wire_event};
use crate::Position;
use crate::api::EventBrokerApi;
use crate::api::{ControlCode, SeekPosition, WireFrame};
use crate::ids::SubscriptionId;
use futures_util::StreamExt;
use std::time::Duration;
use uuid::Uuid;

/// Read the next stream frame within a bounded timeout (live streams never end).
async fn next_frame(
    s: &mut crate::api::FrameStream,
) -> Option<Result<WireFrame, crate::error::EventBrokerError>> {
    tokio::time::timeout(Duration::from_millis(200), s.next())
        .await
        .unwrap_or_default()
}

/// Scenario: consumer/stream/1.01-positive-stream-multipart-frames.md
///
/// A seeded subscription's stream emits one `Event` frame per event in offset-
/// monotonic order, then a `Heartbeat` once the backlog drains. (The open-time
/// `Topology` baseline is the guaranteed first frame in the mock.)
#[tokio::test]
async fn s1_01_positive_stream_multipart_frames() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    h.set_heartbeat_interval(Duration::from_millis(20)).await;
    let c = ctx();
    // Three events → sequences 1, 2, 3 (1-based).
    for _ in 0..3 {
        broker
            .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
            .await
            .unwrap();
    }
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;
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

    let mut s = broker.stream(&c, sub.subscription_id).await.unwrap();
    // First frame: the open-time topology baseline.
    assert!(
        matches!(
            next_frame(&mut s).await,
            Some(Ok(WireFrame::Topology { .. }))
        ),
        "first frame must be the topology baseline"
    );

    // Next three event frames carry sequences 0, 1, 2 in strict order.
    let mut sequences = Vec::new();
    let mut saw_heartbeat = false;
    for _ in 0..20 {
        match next_frame(&mut s).await {
            Some(Ok(WireFrame::Event(e))) => sequences.push(e.sequence.as_i64()),
            Some(Ok(WireFrame::Heartbeat { .. })) => {
                if sequences.len() == 3 {
                    saw_heartbeat = true;
                    break;
                }
            }
            Some(Ok(_)) => {}
            _ => break,
        }
    }
    assert_eq!(
        sequences,
        vec![1, 2, 3],
        "events delivered in offset-monotonic order (1-based)"
    );
    assert!(saw_heartbeat, "a heartbeat arrives once the backlog drains");
}

/// Scenario: consumer/stream/1.02-positive-stream-heartbeat-cadence.md
///
/// An idle subscription (no matching events) emits `Heartbeat` frames at the
/// configured cadence. With a tiny cadence, several arrive in a short window.
#[tokio::test]
async fn s1_02_positive_stream_heartbeat_cadence() {
    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    h.set_heartbeat_interval(Duration::from_millis(10)).await;
    let c = ctx();
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;
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

    let mut s = broker.stream(&c, sub.subscription_id).await.unwrap();
    let _ = next_frame(&mut s).await; // topology baseline

    let mut heartbeats = 0;
    for _ in 0..40 {
        match next_frame(&mut s).await {
            Some(Ok(WireFrame::Heartbeat { at })) => {
                assert!(!at.is_empty(), "heartbeat carries an ISO-8601 timestamp");
                heartbeats += 1;
                if heartbeats >= 3 {
                    break;
                }
            }
            Some(Ok(_)) => {}
            _ => break,
        }
    }
    assert!(
        heartbeats >= 3,
        "at least 3 heartbeats arrive on an idle stream"
    );
}

/// Scenario: consumer/stream/1.03-positive-stream-topology-frame-on-rebalance.md
///
/// When a second consumer JOINs and the rebalance makes A LOSE partitions (not
/// gain), A's open stream receives a non-terminal `Topology` frame with the reduced
/// assignment and the stream stays open.
#[tokio::test]
async fn s1_03_positive_stream_topology_frame_on_rebalance() {
    let (broker, _h) = broker_with_topic(TOPIC, 4).await;
    let c = ctx();
    let c2 = ctx2();
    let gid = make_group(&c, &broker).await;
    let sub_a = join_group(&c, &broker, &gid, TOPIC).await;
    super::super::helpers::seek_all_earliest(&c, &broker, &sub_a).await;

    let mut s = broker.stream(&c, sub_a.subscription_id).await.unwrap();
    let _ = next_frame(&mut s).await; // open-time baseline (4 partitions)

    // B JOINs → rebalance to 2/2; A loses 2 partitions.
    let _sub_b = join_group(&c2, &broker, &gid, TOPIC).await;

    let mut reduced = None;
    for _ in 0..50 {
        match next_frame(&mut s).await {
            Some(Ok(WireFrame::Topology {
                topology_version,
                assigned,
            })) => {
                assert_eq!(topology_version, 2, "topology_version advances to 2");
                reduced = Some(assigned);
                break;
            }
            Some(Ok(WireFrame::Control { .. })) => {
                panic!("a loss must NOT emit a control frame")
            }
            Some(Ok(_)) => {}
            _ => {}
        }
    }
    let reduced = reduced.expect("a loss yields a non-terminal topology frame");
    assert_eq!(reduced.len(), 2, "A's reduced assignment is 2 partitions");
    // Stream stays open (not terminated): a second stream is rejected with
    // StreamingInProgress (409) - NOT SubscriptionTerminated (410). The 409
    // proves the first stream is still active and the subscription is alive.
    let err = broker
        .stream(&c, sub_a.subscription_id)
        .await
        .err()
        .expect("second concurrent stream must be rejected");
    assert!(
        matches!(
            err,
            crate::error::EventBrokerError::StreamingInProgress { .. }
        ),
        "expected StreamingInProgress (still open, not terminated), got {err:?}"
    );
}

/// Scenario: consumer/stream/1.04-negative-stream-positions-not-set.md
///
/// Opening `:stream` before seeding any assigned partition is rejected with
/// `PositionsNotSet` (the SDK analog of the 409 cursor_missing backstop). A
/// well-behaved consumer SEEKs every assigned partition first.
#[tokio::test]
async fn s1_04_negative_stream_positions_not_set() {
    let (broker, _h) = broker_with_topic(TOPIC, 4).await;
    let c = ctx();
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;
    // No SEEK performed - every assigned partition lacks a committed cursor.

    let err = broker
        .stream(&c, sub.subscription_id)
        .await
        .err()
        .expect("opening an unseeded stream must be rejected");
    match err {
        crate::error::EventBrokerError::PositionsNotSet { unseeded, .. } => {
            assert_eq!(unseeded.len(), 4, "all 4 assigned partitions are unseeded");
        }
        other => panic!("expected PositionsNotSet, got {other:?}"),
    }
}

/// Scenario: consumer/stream/1.05-negative-stream-unknown-subscription.md
///
/// Opening `:stream` with a subscription id that never existed is rejected before
/// any stream object or active-stream marker is created.
#[tokio::test]
async fn s1_05_negative_stream_unknown_subscription() {
    let (broker, _h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    let unknown = SubscriptionId(Uuid::new_v4());

    let err = broker
        .stream(&c, unknown)
        .await
        .err()
        .expect("unknown subscription must be rejected");
    assert!(
        matches!(
            err,
            crate::error::EventBrokerError::SubscriptionNotFound { .. }
        ),
        "expected SubscriptionNotFound, got {err:?}"
    );
}

/// Scenario: consumer/stream/1.06-negative-stream-terminated-subscription.md
///
/// Opening `:stream` on a subscription terminated by a gain/lose-all rebalance is
/// rejected (`410`-equivalent). The mock guards this in `stream()` via the
/// `terminated` flag → `EventBrokerError`.
#[tokio::test]
async fn s1_06_negative_stream_terminated_subscription() {
    let (broker, _h) = broker_with_topic(TOPIC, 4).await;
    let c = ctx();
    let c2 = ctx2();
    let gid = make_group(&c, &broker).await;
    let sub_a = join_group(&c, &broker, &gid, TOPIC).await;
    // Seed while A still owns all 4 (before B joins) - the group cursors then
    // survive the rebalance for the partitions A keeps.
    super::super::helpers::seek_all_earliest(&c, &broker, &sub_a).await;
    let sub_b = join_group(&c2, &broker, &gid, TOPIC).await;

    // A is streaming; B leaves → A gains → A is terminated.
    let mut s = broker.stream(&c, sub_a.subscription_id).await.unwrap();
    let _ = next_frame(&mut s).await; // baseline
    broker.leave(&c2, sub_b.subscription_id).await.unwrap();
    // Drive the stream until it closes (terminal control frame then end).
    for _ in 0..50 {
        match next_frame(&mut s).await {
            Some(Ok(WireFrame::Control {
                code: ControlCode::Terminal,
                ..
            })) => break,
            Some(_) => {}
            None => break,
        }
    }

    // Reopening the terminated id is rejected (410-equivalent).
    assert!(
        broker.stream(&c, sub_a.subscription_id).await.is_err(),
        "reopening a terminated subscription is rejected (410-equivalent)"
    );
}

/// Scenario: consumer/stream/1.11-negative-streaming-in-progress.md
///
/// A subscription permits only one active stream at a time. A second concurrent
/// stream open is rejected with `StreamingInProgress`.
#[tokio::test]
async fn s1_11_negative_streaming_in_progress() {
    let (broker, _h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;
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

    let _first = broker.stream(&c, sub.subscription_id).await.unwrap();
    // A second concurrent stream on the same subscription is rejected.
    let err = broker
        .stream(&c, sub.subscription_id)
        .await
        .err()
        .expect("second concurrent stream must be rejected");
    assert!(
        matches!(
            err,
            crate::error::EventBrokerError::StreamingInProgress { .. }
        ),
        "expected StreamingInProgress, got {err:?}"
    );
}

/// Scenario: consumer/stream/1.13-positive-delete-while-streaming.md
///
/// DELETE (mapped to `leave` in the mock) while a stream is open removes the
/// subscription and the open stream ends. A subsequent open of the same id errors
/// (the subscription no longer exists). No control/topology frame precedes the close.
#[tokio::test]
async fn s1_13_positive_delete_while_streaming() {
    let (broker, _h) = broker_with_topic(TOPIC, 1).await;
    let c = ctx();
    let gid = make_group(&c, &broker).await;
    let sub = join_group(&c, &broker, &gid, TOPIC).await;
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

    let mut s = broker.stream(&c, sub.subscription_id).await.unwrap();
    let _ = next_frame(&mut s).await; // baseline

    // DELETE the subscription (priority interrupt).
    broker.leave(&c, sub.subscription_id).await.unwrap();

    // The open stream ends (the loop `break`s when the subscription is gone) with no
    // terminal control/topology frame.
    let mut closed = false;
    for _ in 0..50 {
        match next_frame(&mut s).await {
            None => {
                closed = true;
                break;
            }
            Some(Ok(WireFrame::Control { .. })) => {
                panic!("DELETE must not emit a control frame before close")
            }
            Some(Ok(_)) => {}
            Some(Err(_)) => {
                closed = true;
                break;
            }
        }
    }
    assert!(closed, "the open stream closes after DELETE");
    assert!(
        broker.list_subscriptions(&c).await.unwrap().is_empty(),
        "the subscription is removed"
    );
}

/// Scenario: consumer/stream/1.14-positive-control-progress-frame.md
///
/// DIVERGENCE (filter-saturated progress control frame is not emitted by the mock):
/// the scenario expects a `Control{code: Progress}` carrying the sparse advanced
/// `last_examined` positions when the filter rejects most events. The mock's
/// `open_stream` emits only `Event`/`Heartbeat`/`Topology`/`Terminal-Control` frames -
/// there is no progress-frame emission path, and the mock applies no server-side type
/// filter (it delivers all stored events on assigned partitions). NOT IMPLEMENTABLE as
/// a Progress-frame assertion; recorded as a divergence. Closest real check: the
/// `WireFrame::Control{ ControlCode::Progress, .. }` variant exists and is constructible
/// (the wire contract is present even though the mock never emits it).
#[tokio::test]
async fn s1_14_positive_control_progress_frame() {
    use crate::api::JoinRequest;
    use crate::api::{BarrierMode, SubscriptionInterest};

    let (broker, h) = broker_with_topic(TOPIC, 1).await;
    h.set_heartbeat_interval(Duration::from_millis(20)).await;
    let c = ctx();
    let gid = make_group(&c, &broker).await;

    // JOIN with a narrow filter: a type pattern that no published event matches.
    let sub = broker
        .join(
            &c,
            JoinRequest {
                group: gid,
                client_agent: "audit/1.0".to_owned(),
                interests: vec![SubscriptionInterest {
                    topic: TOPIC.to_owned(),
                    tenant_id: Uuid::nil(),
                    tenant_depth: crate::api::TenantTraversalDepth::CurrentTenant,
                    barrier_mode: BarrierMode::Respect,
                    types: vec![
                        gts_id!("cf.core.events.event.v1~example.mock.broker.no_match.v1~")
                            .to_owned(),
                    ],
                    filter: None,
                }],
                session_timeout: None,
            },
        )
        .await
        .unwrap();
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

    let mut s = broker.stream(&c, sub.subscription_id).await.unwrap();
    let _ = next_frame(&mut s).await; // open-time topology baseline

    // Append heavily; every event is rejected by the narrow filter.
    for _ in 0..5 {
        broker
            .publish(&c, &wire_event(EVT, c.subject_tenant_id()))
            .await
            .unwrap();
    }

    // The stream emits no Event frames, but a sparse `Control{Progress}` advances
    // the frontier while the delivered offset stays at the seeded floor (0).
    let mut saw_progress = false;
    for _ in 0..30 {
        match next_frame(&mut s).await {
            Some(Ok(WireFrame::Event(_))) => panic!("a filtered event must not be delivered"),
            Some(Ok(WireFrame::Control {
                code: ControlCode::Progress,
                positions,
                reason,
            })) => {
                assert_eq!(
                    positions.len(),
                    1,
                    "sparse: only the drifted partition appears"
                );
                assert_eq!(
                    positions[0].offset,
                    Sequence::assigned(0),
                    "delivered offset stays at the seeded floor"
                );
                assert!(
                    positions[0].last_examined >= Sequence::assigned(5),
                    "frontier advanced past the filtered events"
                );
                assert!(reason.is_none(), "Progress carries no reason");
                saw_progress = true;
                break;
            }
            Some(Ok(_)) => {} // heartbeat / topology
            _ => {}
        }
    }
    assert!(
        saw_progress,
        "a filter-saturated stream emits a Control{{Progress}} frame"
    );
    // Side effect: the group's offset-adviser frontier advanced past the scanned events.
    assert!(
        h.last_examined(&gid, TOPIC, 0)
            .await
            .unwrap_or(Sequence::assigned(0))
            >= Sequence::assigned(5),
        "broker last_examined frontier advanced"
    );
}
