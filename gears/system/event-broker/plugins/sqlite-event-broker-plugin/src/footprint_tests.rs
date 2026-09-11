//! A process that streams far past its bounds holds a bounded footprint.
//!
//! Two arms over one workload, because a bound that is never reached proves
//! nothing. The first drives a pass after every round and asserts every
//! partition is inside its bounds after every one of them. The second streams
//! exactly the same events with no pass driven at all and asserts the footprint
//! runs orders of magnitude past the bound - which is what makes the first
//! arm's compliance evidence rather than an artifact of a workload too small to
//! reach the bound.
//!
//! Nothing here sleeps or waits on a task. The backend owns no timer, so a
//! round that wanted one pass called `maintain` once and knows exactly one
//! happened.

use chrono::{TimeDelta, Utc};
use event_broker_sdk::models::Event;
use event_broker_sdk::{EventBrokerBackend, RetentionReport, RetentionRequest};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::backend::SqliteEventBackend;
use crate::test_support::test_backend;

const TOPIC: &str = "gts.cf.core.events.topic.v1~x.eb.footprint.topic.v1";
const PARTITIONS: u32 = 4;
/// Events one round writes to each partition. Written in one call, so a round
/// is one append the way a publisher's batch is.
const EVENTS_PER_ROUND: usize = 25;
/// Rounds the workload streams. High enough that the bound is crossed early and
/// then held for the great majority of the run: what is being demonstrated is a
/// steady state, not a single pass that happened to fit.
const ROUNDS: usize = 40;
/// Filler bytes in each event's payload, so a round moves the byte total by a
/// meaningful fraction of the bound.
const PAYLOAD_BYTES: usize = 800;
/// Bytes a partition may hold. Around thirty of the events above, which one
/// round and a half overshoots - so every round after the first few has
/// removal to do.
const MAX_STORED_BYTES: u64 = 32 * 1024;

/// Every event the workload writes, across every partition.
const STREAMED: u64 = PARTITIONS as u64 * EVENTS_PER_ROUND as u64 * ROUNDS as u64;

fn event(partition: u32) -> Event {
    Event {
        id: Uuid::now_v7(),
        type_id: "gts.cf.core.events.event.v1~x.eb.footprint.foo.v1".to_owned(),
        tenant_id: Uuid::now_v7(),
        source: "footprint-tests".to_owned(),
        subject: "s".to_owned(),
        subject_type: "gts.x.eb.footprint.subject.v1~".to_owned(),
        occurred_at: Utc::now(),
        trace_parent: None,
        data: Some(serde_json::json!({ "filler": "x".repeat(PAYLOAD_BYTES) })),
        partition: Some(partition),
        sequence: None,
        sequence_time: None,
        meta: None,
    }
}

/// One round: `EVENTS_PER_ROUND` events into every partition.
async fn stream_round(backend: &SqliteEventBackend) {
    for partition in 0..PARTITIONS {
        let events: Vec<Event> = (0..EVENTS_PER_ROUND).map(|_| event(partition)).collect();
        backend
            .persist(&SecurityContext::anonymous(), TOPIC, partition, &events)
            .await
            .expect("persist succeeds");
    }
}

/// A pass bounded by bytes alone, so the byte bound is unambiguously what
/// removes: no event in this workload is old enough for the duration bound to
/// have any part in the result.
fn bounded_pass(partition: u32) -> RetentionRequest {
    RetentionRequest::for_partition(TOPIC, partition, Utc::now() - TimeDelta::days(3650))
        .max_stored_bytes(MAX_STORED_BYTES)
        .build()
}

/// A pass that cannot remove anything by either bound, used to read a
/// partition's maintained figures without changing them.
fn observing_pass(partition: u32) -> RetentionRequest {
    RetentionRequest::for_partition(TOPIC, partition, Utc::now() - TimeDelta::days(3650)).build()
}

async fn run_pass(backend: &SqliteEventBackend, request: &RetentionRequest) -> RetentionReport {
    backend
        .maintain(&SecurityContext::anonymous(), request)
        .await
        .expect("retention pass succeeds")
}

/// What the driven arm accumulated, counted as it went.
#[derive(Default)]
struct Footprint {
    passes: u64,
    passes_that_removed: u64,
    removed_events: u64,
    peak_resident_events: u64,
    peak_resident_bytes: u64,
    resident_events: u64,
    resident_bytes: u64,
}

#[tokio::test]
async fn a_process_streaming_far_past_its_bounds_holds_a_bounded_footprint() {
    let backend = test_backend().await;
    let mut footprint = Footprint::default();

    for _ in 0..ROUNDS {
        stream_round(&backend).await;
        for partition in 0..PARTITIONS {
            let report = run_pass(&backend, &bounded_pass(partition)).await;
            footprint.passes += 1;
            footprint.removed_events += report.removed_events;
            if report.removed_events > 0 {
                footprint.passes_that_removed += 1;
            }
            assert!(
                report.remaining_bytes <= MAX_STORED_BYTES,
                "partition {partition} held {} bytes after a pass, over its bound of \
                 {MAX_STORED_BYTES}",
                report.remaining_bytes
            );
            footprint.peak_resident_bytes =
                footprint.peak_resident_bytes.max(report.remaining_bytes);
            footprint.peak_resident_events =
                footprint.peak_resident_events.max(report.remaining_events);
        }
    }

    for partition in 0..PARTITIONS {
        let report = run_pass(&backend, &observing_pass(partition)).await;
        footprint.resident_events += report.remaining_events;
        footprint.resident_bytes += report.remaining_bytes;
    }

    // Reclamation ran, and ran throughout. A single pass at the end would
    // satisfy a bound too, and would be a different property entirely.
    assert_eq!(footprint.passes, ROUNDS as u64 * u64::from(PARTITIONS));
    assert!(
        footprint.passes_that_removed * 2 > footprint.passes,
        "only {} of {} passes removed anything; the bound was barely exercised",
        footprint.passes_that_removed,
        footprint.passes
    );
    // Every event either survives or was removed. Counted on both sides - no
    // figure here is the distance between two sequence numbers.
    assert_eq!(
        footprint.removed_events + footprint.resident_events,
        STREAMED,
        "{} removed plus {} resident does not account for the {STREAMED} streamed",
        footprint.removed_events,
        footprint.resident_events
    );
    assert!(
        footprint.peak_resident_bytes <= MAX_STORED_BYTES,
        "peak resident was {} bytes against a bound of {MAX_STORED_BYTES}",
        footprint.peak_resident_bytes
    );
    // Rows follow bytes, but the claim is about both, so both are asserted
    // against figures fixed before the run. Every event carries at least its own
    // filler, so the events a partition held at its peak, charged that much
    // each, still have to fit inside the byte bound.
    assert!(
        footprint.peak_resident_events * PAYLOAD_BYTES as u64 <= MAX_STORED_BYTES,
        "peak resident was {} events, more than a {MAX_STORED_BYTES}-byte bound can \
         hold of events carrying {PAYLOAD_BYTES} bytes of payload each",
        footprint.peak_resident_events
    );
}

#[tokio::test]
async fn the_same_workload_with_no_pass_driven_runs_far_past_the_bound() {
    let backend = test_backend().await;

    for _ in 0..ROUNDS {
        stream_round(&backend).await;
    }

    let mut resident_events = 0;
    let mut worst_partition_bytes = 0;
    for partition in 0..PARTITIONS {
        let report = run_pass(&backend, &observing_pass(partition)).await;
        assert_eq!(
            report.removed_events, 0,
            "the observing pass removes nothing"
        );
        resident_events += report.remaining_events;
        worst_partition_bytes = u64::max(worst_partition_bytes, report.remaining_bytes);
    }

    assert_eq!(
        resident_events, STREAMED,
        "with no pass driven, every streamed event is still stored"
    );
    // The counter-check the other test rests on: this workload really does run
    // far past the bound, so a partition ending inside it there is reclamation
    // having happened, not a workload that never got near it.
    assert!(
        worst_partition_bytes > MAX_STORED_BYTES * 20,
        "the workload has to overshoot the bound by a wide margin for the bounded \
         arm to mean anything, but the largest partition held only \
         {worst_partition_bytes} bytes against a bound of {MAX_STORED_BYTES}"
    );
}
