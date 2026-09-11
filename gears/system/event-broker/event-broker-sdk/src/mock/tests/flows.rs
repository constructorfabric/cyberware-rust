//! Mirrors scenarios/flows/. Tests migrated per mock-reference-alignment.
use super::helpers::*;
#[cfg(test)]
use crate::sequence::Sequence;

use super::helpers::{broker_with_topic, ctx, join_group, make_group, wire_event};
use crate::Position;
use crate::api::{EventBrokerApi, IngestOutcome, SeekPosition, WireFrame};
use futures_util::StreamExt;

/// Scenario: flows/1.01-flow-publish-subscribe-consume.md
///
/// End-to-end transcript: publish three events → create group → JOIN → SEEK
/// earliest → stream the three events in strict offset order → ack via SEEK →
/// re-JOIN on the same group resumes past the ack (no redelivery).
///
/// The reference fixes RF=100 so sequences are 100/101/102; the in-process mock
/// has no retention floor (sequences start at 0), so this asserts the *shape* of
/// the journey - three in-order events, an advancing group cursor, and a
/// re-JOIN that resumes from the committed cursor - rather than the literal
/// 100-based sequences.
#[tokio::test]
async fn s1_01_flow_publish_subscribe_consume() {
    // 4 partitions; all helper events hash to the tenant's partition (single
    // partition exercised), mirroring "all three events hash to partition 0".
    let (broker, h) = broker_with_topic(TOPIC, 4).await;
    let c = ctx();

    // -- Exchanges 1-3: publish three events (persist-confirming path). ----------
    for _ in 0..3 {
        let outcome = broker
            .publish_sync(&c, &wire_event(EVT, c.subject_tenant_id()))
            .await
            .unwrap();
        assert!(
            matches!(outcome, IngestOutcome::Accepted | IngestOutcome::Persisted),
            "publish must be accepted/persisted, got {outcome:?}"
        );
    }

    // Identify the partition the three events landed on.
    let mut partition = None;
    for p in 0..4u32 {
        if h.stored(TOPIC, p).await.len() == 3 {
            partition = Some(p);
            break;
        }
    }
    let partition = partition.expect("all three events must land on a single partition");

    // -- Exchange 4: create the consumer group. ----------------------------------
    let gid = make_group(&c, &broker).await;

    // -- Exchange 5: JOIN - assignment covers all partitions. --------------------
    let sub = join_group(&c, &broker, &gid, TOPIC).await;
    assert_eq!(sub.topology_version, 1, "first JOIN is topology v1");
    assert_eq!(
        sub.assigned.len(),
        4,
        "solo member is assigned all four partitions"
    );

    // -- Exchange 6: SEEK earliest on every assigned partition (required before
    //    streaming - PositionsNotSet otherwise). ---------------------------------
    super::helpers::seek_all_earliest(&c, &broker, &sub).await;
    assert_eq!(
        h.cursor(&gid, TOPIC, partition).await,
        Some(Sequence::assigned(0)),
        "Earliest resolves to 0 in the mock for the populated partition"
    );

    // -- Exchange 7: open the stream and read the three events in order. ---------
    let mut stream = broker.stream(&c, sub.subscription_id).await.unwrap();
    let mut sequences = Vec::new();
    for _ in 0..12 {
        match tokio::time::timeout(std::time::Duration::from_millis(50), stream.next()).await {
            Ok(Some(Ok(WireFrame::Event(we)))) => {
                sequences.push(we.sequence.as_i64());
                if sequences.len() == 3 {
                    break;
                }
            }
            Ok(Some(Ok(_))) => {} // Topology / Heartbeat / Control - skip.
            _ => break,
        }
    }
    drop(stream); // release the stream so SEEK is not "streaming_in_progress".
    assert_eq!(
        sequences,
        vec![1, 2, 3],
        "events delivered in strict offset order (1-based)"
    );

    // -- Exchange 8: ack through the last delivered offset. ----------------------
    broker
        .seek(
            &c,
            sub.subscription_id,
            &[SeekPosition {
                topic: TOPIC.to_owned(),
                partition,
                value: Position::Exact(Sequence::assigned(2)),
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        h.cursor(&gid, TOPIC, partition).await,
        Some(Sequence::assigned(2)),
        "group cursor advances to the acked offset"
    );

    // -- Exchange 9: re-JOIN on the same group resumes from the committed cursor. -
    broker.leave(&c, sub.subscription_id).await.unwrap();
    let _sub2 = join_group(&c, &broker, &gid, TOPIC).await;
    assert_eq!(
        h.cursor(&gid, TOPIC, partition).await,
        Some(Sequence::assigned(2)),
        "re-JOIN carries the committed cursor (no redelivery of 0-2)"
    );
}
