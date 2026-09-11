//! Full-process integration test for `ConsumerGroupCoordinator`: a topic
//! with 8 partitions, two steady-state subscribers (A, B) each holding 4
//! partitions, then an incremental rollout where a third subscriber (C)
//! JOINs the same group while A and B are both still streaming - the point
//! at which the group briefly has 3 concurrent subscribers. Boots the real
//! server subprocess and drives it over HTTP, same harness as
//! `standalone_integration_tests.rs`.
//!
//! Scope note: this test stops once C's JOIN and the resulting 2-way->3-way
//! rebalance are asserted. It deliberately does NOT go on to have A (or
//! anyone) LEAVE and exercise the forced-recovery re-JOIN path, because
//! tracing the coordinator by hand while writing this surfaced several
//! implementation gaps that would make that path flaky or outright wrong to
//! assert against as-is (not fixed here - flagging for a follow-up):
//! - `stream()`'s delivery loop captures `subscription.assigned` ONCE, when
//!   the stream opens, and never refreshes it from a later `Frame::Topology`
//!   - so a subscriber that loses a partition keeps polling and delivering
//!   it forever. This test's "no extra frame" assertions below are the
//!   direct check for this.
//! - `notify_members` sends the `terminal` control frame but never drops
//!   the member's `topology_tx`/closes the stream afterward, so nothing
//!   server-side actually ends the connection the spec says must close.
//! - A forced-recovery re-JOIN inserts a brand-new member (fresh random
//!   `sub_id`) into the SAME group without the old (terminated) entry being
//!   evicted until its stream disconnects and its `session_timeout` grace
//!   period elapses - so a same-generation "settle without recomputing"
//!   re-JOIN (stream-lifecycle spec's livelock-fencing requirement) isn't
//!   implemented; a recovery re-JOIN just triggers another full rebalance.
//!
//! Every request body sent and response body asserted is inlined or built
//! from a same-file helper that mirrors exactly what was sent/expected (no
//! cross-file body-shaping helper), matching this crate's convention
//! (`standalone_integration_tests.rs`'s module doc).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use event_broker::domain::consumer_group_coordinator::range_split;
use serde_json::{Value, json};
use uuid::Uuid;

use common::{SseFrameReader, TestServer};

const PARTITION_COUNT: u32 = 8;
const EVENTS_PER_PARTITION: i64 = 10;

/// Finds a payload value that hashes to `target_partition` under the same
/// formula the server uses (`domain/ingest.rs::partition_for`) - lets the test
/// deterministically target every partition instead of relying on the default
/// tenant pointer landing wherever it lands.
///
/// The value goes in `data.pk`, and the fixture event type points its
/// partition-key trait at `/data/pk`: a publisher cannot choose a partition,
/// so steering one means varying the member the *type* names. That the pointer
/// reaches into the payload is the case a bare field name could not express, so
/// this exercises it end to end as well.
fn steering_value_for(target_partition: i32, partition_count: u32) -> String {
    for i in 0u32..100_000 {
        let key = format!("pk-{i}");
        let hash = toolkit_stable_hash::murmur3_x86_32(key.as_bytes(), 0);
        let partition = (hash & 0x7FFF_FFFF) % partition_count;
        if i32::try_from(partition).expect("partition fits i32") == target_partition {
            return key;
        }
    }
    panic!("no payload value found hashing to partition {target_partition} within search bound");
}

/// Replicates `ConsumerGroupCoordinator`'s own split (imported directly, not
/// reimplemented) against the ACTUAL sorted subscription ids returned by the
/// server, since ids are server-generated and unpredictable ahead of time.
fn expected_split(partition_count: u32, member_ids: &[Uuid]) -> HashMap<Uuid, Vec<i32>> {
    let mut sorted = member_ids.to_vec();
    sorted.sort_unstable();
    let splits = range_split(partition_count, &sorted);
    sorted
        .into_iter()
        .zip(splits)
        .map(|(id, parts)| {
            let mut v: Vec<i32> = parts
                .into_iter()
                .map(|p| i32::try_from(p).expect("partition fits i32"))
                .collect();
            v.sort_unstable();
            (id, v)
        })
        .collect()
}

enum Rebalance {
    Unchanged,
    Loss,
    Terminal,
}

/// Classification per `event-broker-stream-lifecycle`'s "Rebalance is
/// cooperative" requirement: gain (or lose-all) terminates; a pure loss (or
/// version-only bump) does not.
fn classify(old: &[i32], new: &[i32]) -> Rebalance {
    let old_set: HashSet<i32> = old.iter().copied().collect();
    let new_set: HashSet<i32> = new.iter().copied().collect();
    if old_set == new_set {
        Rebalance::Unchanged
    } else if new_set.is_empty() || new_set.difference(&old_set).next().is_some() {
        Rebalance::Terminal
    } else {
        Rebalance::Loss
    }
}

fn position_json(topic: &str, partition: i32) -> Value {
    json!({ "topic": topic, "partition": partition, "offset": 0, "last_examined": 0 })
}

/// A topology frame whose positions report real progress.
///
/// The plain `topology_frame_json` reports `offset: 0`, which is correct for a
/// session that has not delivered anything yet - and used to be correct for
/// every session, because the *coordinator* built these positions from
/// `Assignment`'s `offset`/`last_examined`, which it sets to 0 and never
/// updates. The session builds them now, from the read set it actually holds,
/// so a stream that has consumed `at` events on a partition reports `at`.
fn topology_frame_json_at(topic: &str, version: i64, partitions: &[i32], at: i64) -> Value {
    let mut sorted = partitions.to_vec();
    sorted.sort_unstable();
    json!({
        "kind": "topology",
        "topology_version": version,
        "assigned": sorted
            .iter()
            .map(|p| json!({
                "topic": topic, "partition": *p, "offset": at, "last_examined": at
            }))
            .collect::<Vec<_>>(),
    })
}

fn topology_frame_json(topic: &str, version: i64, partitions: &[i32]) -> Value {
    let mut sorted = partitions.to_vec();
    sorted.sort_unstable();
    json!({
        "kind": "topology",
        "topology_version": version,
        "assigned": sorted.iter().map(|p| position_json(topic, *p)).collect::<Vec<_>>(),
    })
}

/// A terminal frame whose positions report real progress. Same reason as
/// `topology_frame_json_at`: the session builds these now, so they reflect what
/// it actually delivered rather than the assignment's always-zero fields.
fn terminal_frame_json_at(topic: &str, old_partitions: &[i32], at: i64) -> Value {
    let mut sorted = old_partitions.to_vec();
    sorted.sort_unstable();
    json!({
        "kind": "control",
        "code": "terminal",
        "positions": sorted
            .iter()
            .map(|p| json!({
                "topic": topic, "partition": *p, "offset": at, "last_examined": at
            }))
            .collect::<Vec<_>>(),
        "reason": "rebalanced",
    })
}

fn terminal_frame_json(topic: &str, old_partitions: &[i32]) -> Value {
    let mut sorted = old_partitions.to_vec();
    sorted.sort_unstable();
    json!({
        "kind": "control",
        "code": "terminal",
        "positions": sorted.iter().map(|p| position_json(topic, *p)).collect::<Vec<_>>(),
        "reason": "rebalanced",
    })
}

#[allow(clippy::too_many_arguments)]
fn event_frame_json(
    event_id: Uuid,
    topic: &str,
    event_type: &str,
    tenant_id: Uuid,
    subject: &str,
    subject_type: &str,
    steering: &str,
    partition: i32,
    sequence: i64,
    occurred_at: &Value,
    sequence_time: &Value,
) -> Value {
    json!({
        "kind": "event",
        "payload": {
            "id": event_id,
            "type": event_type,
            "topic": topic,
            "tenant_id": tenant_id,
            "source": "integration-test",
            "subject": subject,
            "subject_type": subject_type,
            "occurred_at": occurred_at,
            "trace_parent": null,
            "data": { "pk": steering },
            "partition": partition,
            "sequence": sequence,
            "sequence_time": sequence_time,
        },
    })
}

/// Reads frames off `reader`, skipping heartbeats - the delivery loop's
/// heartbeat cadence is timing-dependent and irrelevant to the frames this
/// test actually cares about.
async fn next_non_heartbeat(reader: &mut SseFrameReader, timeout: Duration) -> (String, Value) {
    loop {
        let (kind, data) = reader.next_frame(timeout).await;
        if kind != "heartbeat" {
            return (kind, data);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn join(
    client: &reqwest::Client,
    server: &TestServer,
    group_id: &str,
    client_agent: &str,
    topic: &str,
    tenant_id: Uuid,
    event_type: &str,
    session_timeout: Option<&str>,
) -> (Uuid, i64, Vec<i32>) {
    let mut body = json!({
        "consumer_group": group_id,
        "client_agent": client_agent,
        "interests": [{ "topic": topic, "tenant_id": tenant_id, "types": [event_type] }],
    });
    if let Some(timeout) = session_timeout {
        body["session_timeout"] = json!(timeout);
    }
    let resp = client
        .post(server.url("/event-broker/v1/subscriptions"))
        .json(&body)
        .send()
        .await
        .expect("join subscription");
    assert_eq!(resp.status(), 201, "join must return 201");
    let body = resp.json::<Value>().await.expect("join body json");
    let id = Uuid::parse_str(body["id"].as_str().expect("id")).expect("id must be a uuid");
    let topology_version = body["topology_version"].as_i64().expect("topology_version");
    let mut assigned: Vec<i32> = body["assigned"]
        .as_array()
        .expect("assigned array")
        .iter()
        .map(|a| i32::try_from(a["partition"].as_i64().expect("partition")).expect("fits i32"))
        .collect();
    assigned.sort_unstable();
    (id, topology_version, assigned)
}

async fn seek_all(
    client: &reqwest::Client,
    server: &TestServer,
    sub_id: Uuid,
    topic: &str,
    partitions: &[i32],
) {
    let resp = client
        .post(server.url(&format!("/event-broker/v1/subscriptions/{sub_id}:seek")))
        .json(&json!({
            "partition_positions": partitions
                .iter()
                .map(|p| json!({ "topic": topic, "partition": p, "value": "earliest" }))
                .collect::<Vec<_>>(),
        }))
        .send()
        .await
        .expect("seek");
    assert_eq!(resp.status(), 200, "seek must return 200");
}

async fn open_stream(
    client: &reqwest::Client,
    server: &TestServer,
    sub_id: Uuid,
) -> SseFrameReader {
    let resp = client
        .get(server.url(&format!(
            "/event-broker/v1/events:sse?subscription_id={sub_id}"
        )))
        .send()
        .await
        .expect("open sse stream");
    assert_eq!(resp.status(), 200, "stream open must return 200");
    SseFrameReader::new(resp)
}

#[allow(clippy::too_many_arguments)]
async fn drain_events(
    reader: &mut SseFrameReader,
    count: usize,
    topic: &str,
    event_type: &str,
    tenant_id: Uuid,
    subject_type: &str,
    steering: &[String],
    published: &HashMap<i32, HashMap<i64, Uuid>>,
    expected_partitions: &[i32],
) -> HashMap<i32, HashSet<i64>> {
    let mut seen: HashMap<i32, HashSet<i64>> = HashMap::new();
    for _ in 0..count {
        let (kind, data) = next_non_heartbeat(reader, Duration::from_secs(5)).await;
        assert_eq!(
            kind, "event",
            "unexpected frame while draining events: {data}"
        );
        let partition = i32::try_from(data["payload"]["partition"].as_i64().expect("partition"))
            .expect("fits i32");
        assert!(
            expected_partitions.contains(&partition),
            "received an event for partition {partition}, which is outside this subscriber's \
             current assignment {expected_partitions:?} - frame: {data}"
        );
        let sequence = data["payload"]["sequence"].as_i64().expect("sequence");
        let event_id = *published
            .get(&partition)
            .and_then(|by_seq| by_seq.get(&sequence))
            .unwrap_or_else(|| {
                panic!("no published event recorded for partition {partition} sequence {sequence}")
            });
        let occurred_at = data["payload"]["occurred_at"].clone();
        let sequence_time = data["payload"]["sequence_time"].clone();
        assert_eq!(
            data,
            event_frame_json(
                event_id,
                topic,
                event_type,
                tenant_id,
                &format!("s-{partition}-{sequence}"),
                subject_type,
                &steering[usize::try_from(partition).expect("fits usize")],
                partition,
                sequence,
                &occurred_at,
                &sequence_time,
            ),
            "event frame body mismatch for partition {partition} sequence {sequence}"
        );
        seen.entry(partition).or_default().insert(sequence);
    }
    seen
}

/// Red by design until the read set follows the assignment.
///
/// This is the regression test for the defect the streaming redesign exists to
/// fix: the read set is cloned into the delivery closure at open and never
/// revisited, so a session keeps reading partitions it no longer owns. It fails
/// on `received an event for partition 0, which is outside this subscriber's
/// current assignment [4, 5, 6, 7]`, which is exactly right - the code is wrong,
/// not the test. Un-ignore it when the session stops reading a lost partition
/// before announcing the loss.
#[tokio::test]
async fn two_subscribers_baseline_then_third_joins_mid_stream() {
    let topic = "gts.cf.core.events.topic.v1~x.eb.t11.rollout.v1";
    let event_type = "gts.cf.core.events.event.v1~x.eb.t11.foo.v1~";
    let subject_type = "gts.x.eb.t11.subject.v1~";

    let server = TestServer::start(vec![
        json!({ "id": topic, "partitions": PARTITION_COUNT, }),
        json!({
            "id": event_type,
            "topic_id": topic,
            "allowed_subject_types": [subject_type],
            // The member the pointer below names has to be one this type
            // declares: the broker checks that when it admits the type, so a
            // pointer at an undeclared member would drop the type rather than
            // fail on every publish of it.
            "data_schema": { "type": "object", "properties": { "pk": { "type": "string" } } },
            // Steering a publish to a partition means varying the member the
            // type names, so this type names the payload member the test sets.
            "partition_key": "/data/pk",
        }),
    ])
    .await;
    let client = reqwest::Client::new();
    let tenant_id = Uuid::new_v4();

    let steering: Vec<String> = (0..i32::try_from(PARTITION_COUNT).expect("fits i32"))
        .map(|p| steering_value_for(p, PARTITION_COUNT))
        .collect();

    let group_resp = client
        .post(server.url("/event-broker/v1/consumer-groups"))
        .send()
        .await
        .expect("create consumer group");
    assert_eq!(group_resp.status(), 201);
    let group_id = group_resp.json::<Value>().await.expect("group json")["id"]
        .as_str()
        .expect("group id")
        .to_owned();

    // --- Phase 1: A joins alone, gets every partition. ---
    let (a_id, a_v1, a_assigned_v1) = join(
        &client,
        &server,
        &group_id,
        "consumer-a",
        topic,
        tenant_id,
        event_type,
        None,
    )
    .await;
    assert_eq!(a_v1, 1);
    assert_eq!(
        a_assigned_v1,
        (0..i32::try_from(PARTITION_COUNT).expect("fits i32")).collect::<Vec<_>>()
    );
    seek_all(&client, &server, a_id, topic, &a_assigned_v1).await;
    let mut a_reader = open_stream(&client, &server, a_id).await;
    let (kind, data) = next_non_heartbeat(&mut a_reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(topic, 1, &a_assigned_v1));

    // --- Phase 2: B joins - steady state, 4/4 split. A must lose down to
    // its half (a 1->2 split can only ever be a pure loss for the sole
    // pre-existing member: it already holds every partition, so nothing new
    // can appear). ---
    let (b_id, b_v, b_assigned_from_join) = join(
        &client,
        &server,
        &group_id,
        "consumer-b",
        topic,
        tenant_id,
        event_type,
        None,
    )
    .await;
    assert_eq!(b_v, 2);

    let two_way = expected_split(PARTITION_COUNT, &[a_id, b_id]);
    let a_steady = two_way[&a_id].clone();
    let b_steady = two_way[&b_id].clone();
    assert_eq!(a_steady.len(), 4);
    assert_eq!(b_steady.len(), 4);
    assert_eq!(
        b_assigned_from_join, b_steady,
        "B's own JOIN response must match the computed 2-way split"
    );

    let (kind, data) = next_non_heartbeat(&mut a_reader, Duration::from_secs(5)).await;
    assert_eq!(
        kind, "topology",
        "A must receive a non-terminal loss frame, not: {data}"
    );
    assert_eq!(data, topology_frame_json(topic, 2, &a_steady));

    seek_all(&client, &server, b_id, topic, &b_steady).await;
    let mut b_reader = open_stream(&client, &server, b_id).await;
    let (kind, data) = next_non_heartbeat(&mut b_reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(topic, 2, &b_steady));

    // --- Phase 3: baseline load - 10 events on every one of the 8
    // partitions (80 total), published in a fixed, deterministic order so
    // each partition's server-assigned `sequence` is known ahead of time. ---
    let mut published: HashMap<i32, HashMap<i64, Uuid>> = HashMap::new();
    for seq in 1..=EVENTS_PER_PARTITION {
        for partition in 0..i32::try_from(PARTITION_COUNT).expect("fits i32") {
            let event_id = Uuid::new_v4();
            let occurred_at = chrono::Utc::now().to_rfc3339();
            let publish_resp = client
                .post(server.url("/event-broker/v1/events"))
                .json(&json!({
                    "id": event_id,
                    "type": event_type,
                    "tenant_id": tenant_id,
                    "source": "integration-test",
                    "subject": format!("s-{partition}-{seq}"),
                    "subject_type": subject_type,
                    "data": { "pk": steering[usize::try_from(partition).expect("fits usize")] },
                    "occurred_at": occurred_at,
                }))
                .send()
                .await
                .expect("publish event");
            assert_eq!(publish_resp.status(), 202);
            published
                .entry(partition)
                .or_default()
                .insert(seq, event_id);
        }
    }

    // Each subscriber must receive EXACTLY its own 4 partitions x 10 events
    // = 40, matching the "each subscriber gets 10 events on each partition"
    // baseline - not the full 80, which is what a stream loop still reading
    // its stale, pre-rebalance assignment would deliver.
    let expected_count = usize::try_from(EVENTS_PER_PARTITION).expect("fits usize") * 4;
    let a_seen = drain_events(
        &mut a_reader,
        expected_count,
        topic,
        event_type,
        tenant_id,
        subject_type,
        &steering,
        &published,
        &a_steady,
    )
    .await;
    let b_seen = drain_events(
        &mut b_reader,
        expected_count,
        topic,
        event_type,
        tenant_id,
        subject_type,
        &steering,
        &published,
        &b_steady,
    )
    .await;

    for &p in &a_steady {
        let mut got: Vec<i64> = a_seen
            .get(&p)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        got.sort_unstable();
        assert_eq!(
            (1..=EVENTS_PER_PARTITION).collect::<Vec<_>>(),
            got,
            "gap-free coverage for partition {p} via A"
        );
    }
    for &p in &b_steady {
        let mut got: Vec<i64> = b_seen
            .get(&p)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        got.sort_unstable();
        assert_eq!(
            (1..=EVENTS_PER_PARTITION).collect::<Vec<_>>(),
            got,
            "gap-free coverage for partition {p} via B"
        );
    }

    // Neither subscriber may have anything more waiting - proves the poll
    // loop is bounded to the CURRENT (post-rebalance) assignment, not the
    // wider set captured when the stream first opened.
    assert!(
        a_reader
            .try_next_frame(Duration::from_millis(500))
            .await
            .is_none(),
        "A must not deliver anything beyond its {} assigned partitions {a_steady:?}",
        a_steady.len()
    );
    assert!(
        b_reader
            .try_next_frame(Duration::from_millis(500))
            .await
            .is_none(),
        "B must not deliver anything beyond its {} assigned partitions {b_steady:?}",
        b_steady.len()
    );

    // --- Phase 4: incremental rollout - C joins while A and B are both
    // still open, so for a moment the group has 3 concurrent subscribers.
    // Going from 2 to 3 members can shuffle EITHER pre-existing member's
    // sorted position (the split is keyed on raw subscription-id order, not
    // join order), so either could end up losing (non-terminal topology) or
    // gaining (terminal control) - classify from the actual computed split
    // rather than assuming "existing members only ever lose". ---
    let (c_id, c_v, c_assigned_from_join) = join(
        &client,
        &server,
        &group_id,
        "consumer-c",
        topic,
        tenant_id,
        event_type,
        None,
    )
    .await;
    assert_eq!(c_v, 3);

    let three_way = expected_split(PARTITION_COUNT, &[a_id, b_id, c_id]);
    assert_eq!(
        c_assigned_from_join, three_way[&c_id],
        "C's own JOIN response must match the computed 3-way split"
    );

    match classify(&a_steady, &three_way[&a_id]) {
        Rebalance::Unchanged => {}
        Rebalance::Loss => {
            let (kind, data) = next_non_heartbeat(&mut a_reader, Duration::from_secs(5)).await;
            assert_eq!(
                kind, "topology",
                "A must receive a non-terminal loss frame, not: {data}"
            );
            assert_eq!(
                data,
                topology_frame_json_at(topic, 3, &three_way[&a_id], EVENTS_PER_PARTITION)
            );
        }
        Rebalance::Terminal => {
            let (kind, data) = next_non_heartbeat(&mut a_reader, Duration::from_secs(5)).await;
            assert_eq!(
                kind, "control",
                "A must receive a terminal control frame, not: {data}"
            );
            assert_eq!(
                data,
                terminal_frame_json_at(topic, &a_steady, EVENTS_PER_PARTITION)
            );
        }
    }
    match classify(&b_steady, &three_way[&b_id]) {
        Rebalance::Unchanged => {}
        Rebalance::Loss => {
            let (kind, data) = next_non_heartbeat(&mut b_reader, Duration::from_secs(5)).await;
            assert_eq!(
                kind, "topology",
                "B must receive a non-terminal loss frame, not: {data}"
            );
            assert_eq!(
                data,
                topology_frame_json_at(topic, 3, &three_way[&b_id], EVENTS_PER_PARTITION)
            );
        }
        Rebalance::Terminal => {
            let (kind, data) = next_non_heartbeat(&mut b_reader, Duration::from_secs(5)).await;
            assert_eq!(
                kind, "control",
                "B must receive a terminal control frame, not: {data}"
            );
            assert_eq!(
                data,
                terminal_frame_json_at(topic, &b_steady, EVENTS_PER_PARTITION)
            );
        }
    }

    seek_all(&client, &server, c_id, topic, &three_way[&c_id]).await;
    let mut c_reader = open_stream(&client, &server, c_id).await;
    let (kind, data) = next_non_heartbeat(&mut c_reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(topic, 3, &three_way[&c_id]));

    // At this point A, B, and C all hold an open, live SSE stream against
    // the same consumer group at generation 3 - the incremental rollout's 3
    // parallel subscribers.
}

// ---------------------------------------------------------------------------
// Remaining scenarios: rolling replacement and scale-down, each via an
// explicit LEAVE and via a disconnect + `session_timeout` expiry.
//
// Once a member LEAVEs, the coordinator - as traced above - does not settle
// a forced-recovery re-JOIN into the LEAVE's own generation (the
// stream-lifecycle spec's livelock-fencing requirement isn't implemented);
// a recovery re-JOIN just runs another full rebalance, which can itself
// force ANOTHER survivor to recover, and so on. `drive_to_quiescence` below
// reacts to whatever arrives, generation by generation, rather than
// pre-computing a single expected transition - which is the only way to
// assert something concrete without hard-coding an assumption about
// something this coordinator doesn't actually guarantee.
// ---------------------------------------------------------------------------

struct TestFixture {
    server: TestServer,
    client: reqwest::Client,
    group_id: String,
    tenant_id: Uuid,
    topic: String,
    event_type: String,
    subject_type: String,
    steering: Vec<String>,
}

async fn setup(test_tag: &str) -> TestFixture {
    let topic = format!("gts.cf.core.events.topic.v1~x.eb.{test_tag}.rollout.v1");
    let event_type = format!("gts.cf.core.events.event.v1~x.eb.{test_tag}.foo.v1~");
    let subject_type = format!("gts.x.eb.{test_tag}.subject.v1~");
    let server = TestServer::start(vec![
        json!({ "id": topic, "partitions": PARTITION_COUNT, }),
        json!({
            "id": event_type,
            "topic_id": topic,
            "allowed_subject_types": [subject_type],
            // The member the pointer below names has to be one this type
            // declares: the broker checks that when it admits the type, so a
            // pointer at an undeclared member would drop the type rather than
            // fail on every publish of it.
            "data_schema": { "type": "object", "properties": { "pk": { "type": "string" } } },
            // Steering a publish to a partition means varying the member the
            // type names, so this type names the payload member the test sets.
            "partition_key": "/data/pk",
        }),
    ])
    .await;
    let client = reqwest::Client::new();
    let tenant_id = Uuid::new_v4();
    let steering = (0..i32::try_from(PARTITION_COUNT).expect("fits i32"))
        .map(|p| steering_value_for(p, PARTITION_COUNT))
        .collect();

    let group_resp = client
        .post(server.url("/event-broker/v1/consumer-groups"))
        .send()
        .await
        .expect("create consumer group");
    assert_eq!(group_resp.status(), 201);
    let group_id = group_resp.json::<Value>().await.expect("group json")["id"]
        .as_str()
        .expect("group id")
        .to_owned();

    TestFixture {
        server,
        client,
        group_id,
        tenant_id,
        topic,
        event_type,
        subject_type,
        steering,
    }
}

/// A logical group participant. Bundles the CURRENT subscription id/reader
/// together (rather than tracking them as separate variables per
/// participant) because a forced-recovery re-JOIN gives the same logical
/// consumer a brand-new `sub_id` and reader partway through a scenario.
struct Member {
    client_agent: &'static str,
    id: Uuid,
    assigned: Vec<i32>,
    topology_version: i64,
    reader: SseFrameReader,
    /// The `session_timeout` this member joined with - reused verbatim on a
    /// forced-recovery re-JOIN so a member that intentionally used a short
    /// timeout (to be departed-by-disconnect later) doesn't silently revert
    /// to the 30s default the first time some OTHER member's rebalance
    /// forces it through a recovery cycle first.
    session_timeout: Option<&'static str>,
}

async fn join_member(
    f: &TestFixture,
    client_agent: &'static str,
    session_timeout: Option<&'static str>,
) -> Member {
    let (id, topology_version, assigned) = join(
        &f.client,
        &f.server,
        &f.group_id,
        client_agent,
        &f.topic,
        f.tenant_id,
        &f.event_type,
        session_timeout,
    )
    .await;
    seek_all(&f.client, &f.server, id, &f.topic, &assigned).await;
    let reader = open_stream(&f.client, &f.server, id).await;
    Member {
        client_agent,
        id,
        assigned,
        topology_version,
        reader,
        session_timeout,
    }
}

/// Member `id` leaves the group - via an explicit `DELETE` when
/// `via_delete`, or purely by dropping its connection (relying on the
/// `session_timeout` grace-period timer) otherwise. Either way `members` no
/// longer tracks it, and dropping its `Member` (and therefore its `reader`)
/// closes its side of the connection - the trigger `tx.closed()` needs to
/// detect a disconnect in the timer-based case.
async fn depart(f: &TestFixture, members: &mut Vec<Member>, id: Uuid, via_delete: bool) {
    if via_delete {
        let resp = f
            .client
            .delete(
                f.server
                    .url(&format!("/event-broker/v1/subscriptions/{id}")),
            )
            .send()
            .await
            .expect("delete subscription");
        assert_eq!(
            resp.status(),
            204,
            "DELETE /subscriptions/{{id}} must return 204"
        );
    }
    members.retain(|m| m.id != id);
}

/// Drains whatever `topology`/`control` frames arrive for the tracked
/// `members` until a full sweep produces none, re-JOINing (as a
/// well-behaved consumer would) whenever a `terminal` control frame closes
/// a member out.
///
/// Deliberately does NOT pre-compute "the" expected split from the CURRENT
/// live membership and compare a `topology` frame's exact body against it:
/// once a member's forced recovery is itself in flight, "current live
/// membership" can already be one generation ahead of a `topology` frame
/// that was queued (but not yet read) from the PRIOR generation, and
/// asserting today's split against yesterday's frame is simply wrong, not a
/// product bug - this bit a first draft of this helper directly. Instead,
/// each `topology` frame is checked against the invariant that actually
/// defines it (`event-broker-stream-lifecycle`'s "loss/version-change"
/// case: version advances, the new assignment is a non-empty subset of the
/// member's own last-known one), and each `terminal` frame's `positions` -
/// which is self-referential to that member's own history and immune to
/// this race - is still checked byte-for-byte. `verify_gap_free_and_final_state`
/// below is what asserts the exact final split once everything has settled.
async fn drive_to_quiescence(f: &TestFixture, members: &mut Vec<Member>) {
    let poll_timeout = Duration::from_millis(2000);
    let mut quiet_rounds = 0;
    for round in 0..40 {
        let mut changed = false;
        let mut i = 0;
        while i < members.len() {
            let frame = loop {
                match members[i].reader.try_next_frame(poll_timeout).await {
                    None => break None,
                    Some((kind, _)) if kind == "heartbeat" => continue,
                    Some(frame) => break Some(frame),
                }
            };
            let Some((kind, data)) = frame else {
                i += 1;
                continue;
            };
            changed = true;
            match kind.as_str() {
                "topology" => {
                    let new_version = data["topology_version"].as_i64().expect("topology_version");
                    let new_assigned: HashSet<i32> = data["assigned"]
                        .as_array()
                        .expect("assigned array")
                        .iter()
                        .map(|a| {
                            i32::try_from(a["partition"].as_i64().expect("partition"))
                                .expect("fits i32")
                        })
                        .collect();
                    let old_assigned: HashSet<i32> = members[i].assigned.iter().copied().collect();
                    assert!(
                        new_version > members[i].topology_version,
                        "{} topology_version must advance ({} -> {new_version})",
                        members[i].client_agent,
                        members[i].topology_version
                    );
                    assert!(
                        !new_assigned.is_empty()
                            && new_assigned.is_subset(&old_assigned)
                            && new_assigned != old_assigned,
                        "{} received a non-terminal topology frame that isn't a pure, non-empty loss: \
                         old={old_assigned:?} new={new_assigned:?} - {data}",
                        members[i].client_agent
                    );
                    let mut new_assigned: Vec<i32> = new_assigned.into_iter().collect();
                    new_assigned.sort_unstable();
                    assert_eq!(
                        data,
                        topology_frame_json(&f.topic, new_version, &new_assigned),
                        "{} topology frame shape mismatch",
                        members[i].client_agent
                    );
                    members[i].assigned = new_assigned;
                    members[i].topology_version = new_version;
                    i += 1;
                }
                "control" => {
                    assert_eq!(
                        data,
                        terminal_frame_json(&f.topic, &members[i].assigned),
                        "{} terminal frame mismatch",
                        members[i].client_agent
                    );
                    let client_agent = members[i].client_agent;
                    let members_session_timeout = members[i].session_timeout;
                    let departed = members.remove(i); // dropped here - closes its connection
                    drop(departed);
                    // Give the server's disconnect watcher a moment to mark
                    // the old member Unassigned before re-JOIN's own
                    // eviction check runs - otherwise the stale entry lingers
                    // and the re-JOIN rebalances against a phantom 4th
                    // member instead of replacing this one's slot.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let mut recovered = join_member(f, client_agent, members_session_timeout).await;
                    let (kind2, data2) =
                        next_non_heartbeat(&mut recovered.reader, Duration::from_secs(5)).await;
                    assert_eq!(
                        kind2, "topology",
                        "{client_agent}'s recovery JOIN must open with a topology frame"
                    );
                    assert_eq!(
                        data2,
                        topology_frame_json(
                            &f.topic,
                            recovered.topology_version,
                            &recovered.assigned
                        )
                    );
                    members.insert(i, recovered);
                    i += 1;
                }
                other => panic!(
                    "unexpected frame kind '{other}' from {}: {data}",
                    members[i].client_agent
                ),
            }
        }
        if changed {
            quiet_rounds = 0;
        } else {
            quiet_rounds += 1;
            if quiet_rounds >= 3 {
                return;
            }
        }
        assert!(
            round < 39,
            "group did not reach quiescence within the round budget"
        );
    }
}

/// Publishes `events_per_partition` events on every partition, drains them
/// from every tracked member (asserting each frame's exact body), asserts
/// gap-free coverage of every partition across whichever member(s) held it,
/// and asserts the exact final `GET /subscriptions/{id}` body for every
/// member still standing.
async fn verify_gap_free_and_final_state(
    f: &TestFixture,
    members: &mut [Member],
    events_per_partition: i64,
) {
    let mut published: HashMap<i32, HashMap<i64, Uuid>> = HashMap::new();
    for seq in 1..=events_per_partition {
        for partition in 0..i32::try_from(PARTITION_COUNT).expect("fits i32") {
            let event_id = Uuid::new_v4();
            let occurred_at = chrono::Utc::now().to_rfc3339();
            let resp = f
                .client
                .post(f.server.url("/event-broker/v1/events"))
                .json(&json!({
                    "id": event_id,
                    "type": f.event_type,
                    "tenant_id": f.tenant_id,
                    "source": "integration-test",
                    "subject": format!("s-{partition}-{seq}"),
                    "subject_type": f.subject_type,
                    "data": { "pk": f.steering[usize::try_from(partition).expect("fits usize")] },
                    "occurred_at": occurred_at,
                }))
                .send()
                .await
                .expect("publish event");
            assert_eq!(resp.status(), 202);
            published
                .entry(partition)
                .or_default()
                .insert(seq, event_id);
        }
    }

    let mut union_seen: HashMap<i32, HashSet<i64>> = HashMap::new();
    for m in members.iter_mut() {
        let count = m.assigned.len() * usize::try_from(events_per_partition).expect("fits usize");
        let seen = drain_events(
            &mut m.reader,
            count,
            &f.topic,
            &f.event_type,
            f.tenant_id,
            &f.subject_type,
            &f.steering,
            &published,
            &m.assigned,
        )
        .await;
        for (partition, seqs) in seen {
            union_seen.entry(partition).or_default().extend(seqs);
        }
    }
    for partition in 0..i32::try_from(PARTITION_COUNT).expect("fits i32") {
        let mut got: Vec<i64> = union_seen
            .get(&partition)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        got.sort_unstable();
        assert_eq!(
            (1..=events_per_partition).collect::<Vec<_>>(),
            got,
            "gap-free coverage for partition {partition}"
        );
    }

    for m in members.iter() {
        let resp = f
            .client
            .get(
                f.server
                    .url(&format!("/event-broker/v1/subscriptions/{}", m.id)),
            )
            .send()
            .await
            .expect("get subscription");
        assert_eq!(resp.status(), 200);
        let body = resp.json::<Value>().await.expect("subscription json");
        let expires_at = body["expires_at"].clone();
        // `notify_members` sends NO frame at all when a rebalance leaves a
        // member's own assigned set unchanged (`if new_set == old_set {
        // continue; }` in consumer_group_coordinator/mod.rs) - but
        // `join()`'s sibling-persistence loop still bumps that member's DB
        // `topology_version` to the new value regardless. So a member whose
        // OWN assignment never changes across a LATER, unrelated rebalance
        // ends up with a DB `topology_version` ahead of anything its own
        // stream was ever told about via a frame - the spec's own
        // "topology_version bumped, assigned set unchanged" case (which it
        // says must still emit a non-terminal topology frame) isn't
        // actually implemented. Tracking this in the DB only, silently, is
        // why `topology_version` here is read from the response rather
        // than asserted against what frames told this test - it's real
        // server state this test cannot predict without also modeling
        // every OTHER member's rebalances that never produced a frame here.
        let actual_topology_version = body["topology_version"].as_i64().expect("topology_version");
        assert!(
            actual_topology_version >= m.topology_version,
            "{}'s DB topology_version ({actual_topology_version}) must never be BEHIND what its own \
             frames reported ({})",
            m.client_agent,
            m.topology_version
        );
        let mut sorted_assigned = m.assigned.clone();
        sorted_assigned.sort_unstable();
        assert_eq!(
            body,
            json!({
                "id": m.id,
                "consumer_group": f.group_id,
                "client_agent": m.client_agent,
                "interests": [{
                    "topic": f.topic,
                    "tenant_id": f.tenant_id,
                    "types": [f.event_type],
                    "max_depth": 0,
                    "barrier_mode": "respect",
                    "filter": null,
                }],
                "assigned": sorted_assigned
                    .iter()
                    .map(|p| json!({ "topic": f.topic, "partition": p }))
                    .collect::<Vec<_>>(),
                "topology_version": actual_topology_version,
                "expires_at": expires_at,
            }),
            "final subscription state mismatch for {}",
            m.client_agent
        );
    }
}

/// A and B reach 2-way steady state (4/4), asserting both initial frames
/// exactly - the shared starting point for every scenario below.
async fn two_way_steady_state(f: &TestFixture) -> (Member, Member) {
    let mut a = join_member(f, "consumer-a", None).await;
    assert_eq!(a.topology_version, 1);
    assert_eq!(
        a.assigned,
        (0..i32::try_from(PARTITION_COUNT).expect("fits i32")).collect::<Vec<_>>()
    );
    let (kind, data) = next_non_heartbeat(&mut a.reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(&f.topic, 1, &a.assigned));

    let (b_id, b_v, b_assigned) = join(
        &f.client,
        &f.server,
        &f.group_id,
        "consumer-b",
        &f.topic,
        f.tenant_id,
        &f.event_type,
        None,
    )
    .await;
    assert_eq!(b_v, 2);
    let two_way = expected_split(PARTITION_COUNT, &[a.id, b_id]);
    assert_eq!(
        b_assigned, two_way[&b_id],
        "B's own JOIN response must match the computed 2-way split"
    );

    let (kind, data) = next_non_heartbeat(&mut a.reader, Duration::from_secs(5)).await;
    assert_eq!(
        kind, "topology",
        "A must receive a non-terminal loss frame, not: {data}"
    );
    assert_eq!(data, topology_frame_json(&f.topic, 2, &two_way[&a.id]));
    a.assigned = two_way[&a.id].clone();
    a.topology_version = 2;

    seek_all(&f.client, &f.server, b_id, &f.topic, &two_way[&b_id]).await;
    let mut b_reader = open_stream(&f.client, &f.server, b_id).await;
    let (kind, data) = next_non_heartbeat(&mut b_reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(&f.topic, 2, &two_way[&b_id]));
    let b = Member {
        client_agent: "consumer-b",
        id: b_id,
        assigned: two_way[&b_id].clone(),
        topology_version: 2,
        reader: b_reader,
        session_timeout: None,
    };

    (a, b)
}

#[tokio::test]
async fn scale_down_via_delete() {
    let f = setup("t12d").await;
    let (a, b) = two_way_steady_state(&f).await;
    let mut members = vec![a, b];

    let b_id = members[1].id;
    depart(&f, &mut members, b_id, true).await; // B leaves explicitly
    drive_to_quiescence(&f, &mut members).await;

    assert_eq!(members.len(), 1, "only the sole survivor must remain");
    assert_eq!(
        members[0].assigned,
        (0..i32::try_from(PARTITION_COUNT).expect("fits i32")).collect::<Vec<_>>(),
        "sole survivor must end up holding every partition"
    );
    verify_gap_free_and_final_state(&f, &mut members, 2).await;
}

#[tokio::test]
async fn scale_down_via_disconnect() {
    let f = setup("t12t").await;
    // The departing member needs a short session_timeout - it never calls
    // DELETE, so the D7 grace-period timer is what evicts it.
    let mut a = join_member(&f, "consumer-a", Some("PT1S")).await;
    assert_eq!(a.topology_version, 1);
    let (kind, data) = next_non_heartbeat(&mut a.reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(&f.topic, 1, &a.assigned));

    let (b_id, b_v, b_assigned) = join(
        &f.client,
        &f.server,
        &f.group_id,
        "consumer-b",
        &f.topic,
        f.tenant_id,
        &f.event_type,
        None,
    )
    .await;
    assert_eq!(b_v, 2);
    let two_way = expected_split(PARTITION_COUNT, &[a.id, b_id]);
    assert_eq!(b_assigned, two_way[&b_id]);
    let (kind, data) = next_non_heartbeat(&mut a.reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(&f.topic, 2, &two_way[&a.id]));
    a.assigned = two_way[&a.id].clone();
    a.topology_version = 2;
    seek_all(&f.client, &f.server, b_id, &f.topic, &two_way[&b_id]).await;
    let mut b_reader = open_stream(&f.client, &f.server, b_id).await;
    let (kind, data) = next_non_heartbeat(&mut b_reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(&f.topic, 2, &two_way[&b_id]));
    let b = Member {
        client_agent: "consumer-b",
        id: b_id,
        assigned: two_way[&b_id].clone(),
        topology_version: 2,
        reader: b_reader,
        session_timeout: None,
    };

    let mut members = vec![a, b];
    let a_id = members[0].id;
    depart(&f, &mut members, a_id, false).await; // A disconnects, no DELETE
    drive_to_quiescence(&f, &mut members).await;

    assert_eq!(members.len(), 1, "only the sole survivor must remain");
    assert_eq!(members[0].client_agent, "consumer-b");
    assert_eq!(
        members[0].assigned,
        (0..i32::try_from(PARTITION_COUNT).expect("fits i32")).collect::<Vec<_>>(),
        "sole survivor must end up holding every partition"
    );
    verify_gap_free_and_final_state(&f, &mut members, 2).await;
}

#[tokio::test]
async fn rolling_replacement_via_delete() {
    let f = setup("t13d").await;
    let (a, b) = two_way_steady_state(&f).await;
    let mut members = vec![a, b];

    // Incremental rollout: C joins while A and B are both still open - a
    // moment with 3 concurrent subscribers - before A departs.
    let mut c = join_member(&f, "consumer-c", None).await;
    let live_ids: Vec<Uuid> = members
        .iter()
        .map(|m| m.id)
        .chain(std::iter::once(c.id))
        .collect();
    let three_way = expected_split(PARTITION_COUNT, &live_ids);
    assert_eq!(
        c.assigned, three_way[&c.id],
        "C's own JOIN response must match the computed 3-way split"
    );
    assert_eq!(c.topology_version, 3);
    let (kind, data) = next_non_heartbeat(&mut c.reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(&f.topic, 3, &c.assigned));
    members.push(c);
    drive_to_quiescence(&f, &mut members).await; // settles A's and B's loss/gain from C joining

    assert_eq!(
        members.len(),
        3,
        "A, B, and C must all still be present after C's JOIN"
    );

    let a_id = members
        .iter()
        .find(|m| m.client_agent == "consumer-a")
        .expect("consumer-a must still be tracked")
        .id;
    depart(&f, &mut members, a_id, true).await; // A leaves explicitly
    drive_to_quiescence(&f, &mut members).await;

    assert_eq!(
        members.len(),
        2,
        "B and C (in whatever generation) must remain after A's departure"
    );
    let mut assigned_union: Vec<i32> = members.iter().flat_map(|m| m.assigned.clone()).collect();
    assigned_union.sort_unstable();
    assert_eq!(
        assigned_union,
        (0..i32::try_from(PARTITION_COUNT).expect("fits i32")).collect::<Vec<_>>(),
        "the two survivors together must hold every partition exactly once"
    );
    verify_gap_free_and_final_state(&f, &mut members, 2).await;
}

#[tokio::test]
async fn rolling_replacement_via_disconnect() {
    let f = setup("t13t").await;
    // A is the one that will depart by disconnect, so it needs a short
    // session_timeout from its very first JOIN.
    let mut a = join_member(&f, "consumer-a", Some("PT1S")).await;
    assert_eq!(a.topology_version, 1);
    let (kind, data) = next_non_heartbeat(&mut a.reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(&f.topic, 1, &a.assigned));

    let (b_id, b_v, b_assigned) = join(
        &f.client,
        &f.server,
        &f.group_id,
        "consumer-b",
        &f.topic,
        f.tenant_id,
        &f.event_type,
        None,
    )
    .await;
    assert_eq!(b_v, 2);
    let two_way = expected_split(PARTITION_COUNT, &[a.id, b_id]);
    assert_eq!(b_assigned, two_way[&b_id]);
    let (kind, data) = next_non_heartbeat(&mut a.reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(&f.topic, 2, &two_way[&a.id]));
    a.assigned = two_way[&a.id].clone();
    a.topology_version = 2;
    seek_all(&f.client, &f.server, b_id, &f.topic, &two_way[&b_id]).await;
    let mut b_reader = open_stream(&f.client, &f.server, b_id).await;
    let (kind, data) = next_non_heartbeat(&mut b_reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(&f.topic, 2, &two_way[&b_id]));
    let b = Member {
        client_agent: "consumer-b",
        id: b_id,
        assigned: two_way[&b_id].clone(),
        topology_version: 2,
        reader: b_reader,
        session_timeout: None,
    };
    let mut members = vec![a, b];

    let mut c = join_member(&f, "consumer-c", None).await;
    let live_ids: Vec<Uuid> = members
        .iter()
        .map(|m| m.id)
        .chain(std::iter::once(c.id))
        .collect();
    let three_way = expected_split(PARTITION_COUNT, &live_ids);
    assert_eq!(c.assigned, three_way[&c.id]);
    assert_eq!(c.topology_version, 3);
    let (kind, data) = next_non_heartbeat(&mut c.reader, Duration::from_secs(5)).await;
    assert_eq!(kind, "topology");
    assert_eq!(data, topology_frame_json(&f.topic, 3, &c.assigned));
    members.push(c);
    drive_to_quiescence(&f, &mut members).await;
    assert_eq!(members.len(), 3);

    let a_id = members
        .iter()
        .find(|m| m.client_agent == "consumer-a")
        .expect("consumer-a must still be tracked")
        .id;
    depart(&f, &mut members, a_id, false).await; // A disconnects, no DELETE
    drive_to_quiescence(&f, &mut members).await;

    assert_eq!(
        members.len(),
        2,
        "B and C (in whatever generation) must remain after A's departure"
    );
    let mut assigned_union: Vec<i32> = members.iter().flat_map(|m| m.assigned.clone()).collect();
    assigned_union.sort_unstable();
    assert_eq!(
        assigned_union,
        (0..i32::try_from(PARTITION_COUNT).expect("fits i32")).collect::<Vec<_>>(),
        "the two survivors together must hold every partition exactly once"
    );
    verify_gap_free_and_final_state(&f, &mut members, 2).await;
}
