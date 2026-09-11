//! The session's frame sequence, against stubbed partitions.
//!
//! No cache, no storage, no HTTP. What is asserted is the order and content of
//! the frames a consumer would receive, because that is the contract.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use chrono::Utc;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::Instant;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::model::{Assignment, BarrierMode, Event, Sequence, TenantTraversalDepth};
use crate::domain::streaming::assignment::{AssignmentDelta, Generation};
use crate::domain::streaming::filter::{EventFilter, InterestFilter};
use crate::domain::streaming::frames::{CloseReason, ControlCode, Frame};
use crate::domain::streaming::lease::{InProcessStreamLeases, StreamLeases};
use crate::domain::streaming::progress::ProgressConfig;
use crate::domain::streaming::read::{MaxBytes, MaxEvents, PartitionRead, ReadLimit};
use crate::domain::streaming::read_set::{PartitionSlot, ReadSet};
use crate::domain::streaming::reader::PartitionReader;
use crate::domain::streaming::session::{SessionOpening, SessionState, StreamSession};
use crate::domain::streaming::source::PartitionKey;
use crate::domain::streaming::time::NowFn;

const TOPIC: &str = "gts.cf.core.events.topic.v1~x.eb.orders.acme.v1";
const CREATED: &str = "gts.cf.core.events.event.v1~x.eb.orders.created.v1~";

fn gts(id: &str) -> GtsInstanceId {
    GtsInstanceId::try_new(id).expect("static gts id is valid")
}

fn key(partition: i32) -> PartitionKey {
    PartitionKey::new(gts(TOPIC), partition)
}

fn event(tenant: Uuid, sequence: i64) -> Event {
    let sequence = Sequence::assigned(sequence);
    Event {
        id: Uuid::nil(),
        r#type: crate::test_support::event_type_id(CREATED),
        topic: gts(TOPIC),
        tenant_id: tenant,
        source: "session-test".to_owned(),
        subject: "order".to_owned(),
        subject_type: "order".to_owned(),
        occurred_at: Utc::now(),
        trace_parent: None,
        data: json!({}),
        meta: None,
        partition: Some(0),
        sequence: Some(sequence),
        sequence_time: None,
    }
}

/// A partition whose contents a test sets, handing them over once.
struct StubPartition {
    events: Mutex<Vec<Event>>,
    accounted_through: AtomicI64,
    /// How many times the session has asked whether this partition is ready.
    ///
    /// The only observable that separates a parked session from a spinning one:
    /// both emit nothing, so "did a frame arrive" cannot tell them apart.
    probes: AtomicI64,
}

impl StubPartition {
    fn holding(events: Vec<Event>) -> Arc<Self> {
        let through = events
            .iter()
            .filter_map(|event| event.sequence)
            .max()
            .unwrap_or(Sequence::assigned(0));
        Arc::new(Self {
            events: Mutex::new(events),
            accounted_through: AtomicI64::new(through.as_i64()),
            probes: AtomicI64::new(0),
        })
    }

    fn empty() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            accounted_through: AtomicI64::new(0),
            probes: AtomicI64::new(0),
        })
    }
}

/// One stub per partition, because there is one seam. This was a
/// `StubPartition` implementing the read plus a `StubReader` wrapping it for
/// readiness - two handles onto one partition, which production no longer has.
impl PartitionReader for StubPartition {
    fn has_data(&self) -> bool {
        self.probes.fetch_add(1, Ordering::Relaxed);
        !self.events.lock().expect("test lock").is_empty()
    }

    fn seek(&self, _offset: Sequence) {}

    fn report_scanning(&self, _scanning: bool) {}

    fn read(&self, _limit: ReadLimit) -> PartitionRead {
        let drained: Vec<Event> = std::mem::take(&mut *self.events.lock().expect("test lock"));
        if drained.is_empty() {
            return PartitionRead::NothingNew;
        }
        // A real cache hands back borrowed runs; a stub can only build an owned
        // batch, which is why the session takes an `EventBatch` and never a
        // concrete storage type.
        let accounted_through = Sequence::assigned(self.accounted_through.load(Ordering::Relaxed));
        let held = drained.len();
        PartitionRead::Hit {
            events: crate::domain::streaming::read::EventBatch::from_runs(vec![
                crate::domain::streaming::read::EventSlice::builder(drained.into())
                    .range(0..held)
                    .frontier(accounted_through)
                    .build(),
            ]),
            accounted_through,
        }
    }
}

struct Harness {
    session: StreamSession,
    _leases: Arc<InProcessStreamLeases>,
}

/// A real coordinator with one member, so a test session gets a real generation
/// watch and a real membership handle.
///
/// The same choice the lease already makes here: `InProcessStreamLeases` is the
/// production type, not a stub, because it is in-memory and has no I/O.
/// `ConsumerGroupCoordinator` is a `Mutex<HashMap>` for the same reason.
///
/// The handle owns an `Arc` to the coordinator, so nothing else needs to keep it
/// alive.
fn membership(
    partitions: i32,
) -> (
    tokio::sync::watch::Receiver<crate::domain::streaming::assignment::Generation>,
    crate::domain::consumer_group_coordinator::MembershipHandle,
) {
    use crate::domain::consumer_group_coordinator::{ConsumerGroupCoordinator, TopicInterest};

    let groups = Arc::new(ConsumerGroupCoordinator::new());
    let group = gts("gts.cf.core.events.consumer_group.v1~02943530-10da-4624-a3ae-b998c425847f");
    let sub_id = Uuid::new_v4();
    groups.join(
        &group,
        sub_id,
        &[TopicInterest {
            id: gts(TOPIC),
            partitions,
        }],
        Duration::from_secs(30),
    );
    ConsumerGroupCoordinator::subscribe(&groups, &group, sub_id).expect("the member just joined")
}

fn open(partitions: Vec<(i32, Arc<StubPartition>)>, tenant: Uuid) -> Harness {
    let leases = Arc::new(InProcessStreamLeases::new());
    let lease = leases.acquire(Uuid::new_v4()).expect("lease is free");

    let slots = partitions
        .into_iter()
        .map(|(partition, held)| PartitionSlot::new(key(partition), held))
        .collect();

    let filter: Arc<dyn EventFilter> = Arc::new(
        InterestFilter::compile(&[crate::domain::model::Interest {
            topic: gts(TOPIC),
            tenant_id: tenant,
            depth: TenantTraversalDepth::CurrentTenant,
            barrier_mode: BarrierMode::Respect,
            types: vec![CREATED.to_owned()],
            filter: None,
        }])
        .expect("compiles"),
    );

    let now: NowFn = Arc::new(Utc::now);
    let (generations, membership) = membership(8);
    let session = StreamSession::open(SessionOpening {
        read_set: ReadSet::seed(slots),
        filter,
        progress: ProgressConfig::default(),
        heartbeat_interval: Duration::from_secs(5),
        limit: ReadLimit::new(MaxEvents(256), MaxBytes(1024 * 1024)),
        topology_version: 7,
        ready: Arc::new(Notify::new()),
        started_at: Instant::now(),
        now,
        unanswerable_tolerance: Duration::from_secs(30),
        lease,
        generations,
        membership,
    });

    Harness {
        session,
        _leases: leases,
    }
}

/// Collects frames until the stream ends or `limit` frames have arrived,
/// whichever is first. Bounded so a stuck session fails rather than hangs.
async fn drain(session: &mut StreamSession, limit: usize) -> Vec<Frame> {
    let mut frames = Vec::new();
    for _ in 0..limit {
        match tokio::time::timeout(Duration::from_millis(200), session.next_frame()).await {
            Ok(Some(frame)) => frames.push(frame),
            // Either the stream ended or it went quiet. Both mean "no more
            // frames for this assertion", and the bound is what makes a stuck
            // session fail rather than hang.
            Ok(None) | Err(_) => break,
        }
    }
    frames
}

fn kinds(frames: &[Frame]) -> Vec<&'static str> {
    frames
        .iter()
        .map(|frame| match frame {
            Frame::Event(_) => "event",
            Frame::Heartbeat { .. } => "heartbeat",
            Frame::Topology { .. } => "topology",
            Frame::Control {
                code: ControlCode::Progress,
                ..
            } => "progress",
            Frame::Control {
                code: ControlCode::Terminal,
                ..
            } => "terminal",
        })
        .collect()
}

#[tokio::test]
async fn the_first_frame_is_the_open_time_topology_baseline() {
    let tenant = Uuid::new_v4();
    let mut harness = open(vec![(0, StubPartition::empty())], tenant);

    let frames = drain(&mut harness.session, 1).await;

    // The baseline is what lets a consumer attribute every later position to a
    // topology it has actually seen.
    assert_eq!(kinds(&frames), vec!["topology"]);
    assert_eq!(harness.session.state(), SessionState::Streaming);
}

#[tokio::test]
async fn matching_events_are_delivered_one_per_frame_in_order() {
    let tenant = Uuid::new_v4();
    let held = StubPartition::holding(vec![event(tenant, 1), event(tenant, 2), event(tenant, 3)]);
    let mut harness = open(vec![(0, held)], tenant);

    let frames = drain(&mut harness.session, 4).await;

    assert_eq!(kinds(&frames), vec!["topology", "event", "event", "event"]);
    let delivered: Vec<i64> = frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Event(event) => event.sequence,
            _ => None,
        })
        .map(Sequence::as_i64)
        .collect();
    assert_eq!(delivered, vec![1, 2, 3]);
}

#[tokio::test]
async fn an_event_of_another_tenant_is_not_delivered_but_still_advances_the_frontier() {
    let tenant = Uuid::new_v4();
    let held = StubPartition::holding(vec![event(Uuid::new_v4(), 1), event(tenant, 2)]);
    let mut harness = open(vec![(0, held)], tenant);

    let frames = drain(&mut harness.session, 3).await;

    assert_eq!(kinds(&frames), vec!["topology", "event"]);
    let position = harness
        .session
        .positions()
        .into_iter()
        .next()
        .expect("one partition");
    assert_eq!(
        position.offset,
        Sequence::assigned(2),
        "the cursor is the last delivered"
    );
    assert_eq!(
        position.last_examined,
        Sequence::assigned(2),
        "and the frontier covers the rejected one"
    );
}

#[tokio::test]
async fn an_idle_stream_emits_a_heartbeat_rather_than_nothing() {
    tokio::time::pause();
    let tenant = Uuid::new_v4();
    let mut harness = open(vec![(0, StubPartition::empty())], tenant);

    let baseline = harness.session.next_frame().await;
    assert!(matches!(baseline, Some(Frame::Topology { .. })));

    tokio::time::advance(Duration::from_secs(6)).await;
    let next = harness.session.next_frame().await;

    assert!(matches!(next, Some(Frame::Heartbeat { .. })));
}

#[tokio::test]
async fn a_loss_narrows_the_read_set_and_the_stream_continues() {
    let tenant = Uuid::new_v4();
    let mut harness = open(
        vec![
            (0, StubPartition::empty()),
            (1, StubPartition::empty()),
            (2, StubPartition::empty()),
        ],
        tenant,
    );
    let _ = harness.session.next_frame().await;

    let before = Generation::new(7, vec![assignment(0), assignment(1), assignment(2)]);
    let after = Generation::new(8, vec![assignment(0), assignment(1)]);
    harness
        .session
        .apply(&AssignmentDelta::classify(&before, &after));

    let frame = harness.session.next_frame().await.expect("a frame");
    match frame {
        Frame::Topology {
            topology_version,
            positions,
        } => {
            assert_eq!(topology_version, 8);
            // The frame reports what the session will read next, not what it
            // held a moment ago.
            assert_eq!(positions.len(), 2);
        }
        other => panic!("expected a topology frame, got {other:?}"),
    }
    assert_eq!(harness.session.state(), SessionState::Streaming);
}

#[tokio::test]
async fn a_gain_ends_the_stream_with_a_terminal_frame_carrying_the_frontier() {
    let tenant = Uuid::new_v4();
    let held = StubPartition::holding(vec![event(tenant, 41)]);
    let mut harness = open(vec![(0, held)], tenant);
    let _ = drain(&mut harness.session, 2).await;

    harness.session.apply(&AssignmentDelta::Gain);
    let frames = drain(&mut harness.session, 3).await;

    assert_eq!(kinds(&frames), vec!["terminal"]);
    match frames.first() {
        Some(Frame::Control {
            positions, reason, ..
        }) => {
            assert_eq!(*reason, Some(CloseReason::Rebalanced));
            // The frontier goes out with the close, so the consumer can commit
            // before re-joining.
            assert_eq!(
                positions.first().map(|p| p.offset),
                Some(Sequence::assigned(41))
            );
        }
        other => panic!("expected a terminal control frame, got {other:?}"),
    }
    assert_eq!(harness.session.state(), SessionState::Closed);
    assert!(harness.session.next_frame().await.is_none());
}

#[tokio::test]
async fn losing_every_partition_ends_the_stream() {
    let tenant = Uuid::new_v4();
    let mut harness = open(vec![(0, StubPartition::empty())], tenant);
    let _ = harness.session.next_frame().await;

    harness.session.apply(&AssignmentDelta::LoseAll);
    let frame = harness.session.next_frame().await.expect("a frame");

    match frame {
        Frame::Control { reason, .. } => assert_eq!(reason, Some(CloseReason::LoseAll)),
        other => panic!("expected a terminal control frame, got {other:?}"),
    }
}

#[tokio::test]
async fn a_teardown_closes_with_its_own_reason() {
    let tenant = Uuid::new_v4();
    let mut harness = open(vec![(0, StubPartition::empty())], tenant);
    let _ = harness.session.next_frame().await;

    harness.session.tear_down();
    let frame = harness.session.next_frame().await.expect("a frame");

    match frame {
        Frame::Control { reason, .. } => assert_eq!(reason, Some(CloseReason::Teardown)),
        other => panic!("expected a terminal control frame, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unchanged_delta_emits_nothing() {
    let tenant = Uuid::new_v4();
    tokio::time::pause();
    let mut harness = open(vec![(0, StubPartition::empty())], tenant);
    let _ = harness.session.next_frame().await;

    harness.session.apply(&AssignmentDelta::Unchanged);

    // Nothing queued, so the next frame is whatever the cadence produces - not a
    // topology frame for a topology that did not move.
    let next = tokio::time::timeout(Duration::from_millis(50), harness.session.next_frame()).await;
    assert!(next.is_err(), "no frame is owed yet");
}

#[tokio::test]
async fn reads_rotate_over_partitions_that_have_something() {
    let tenant = Uuid::new_v4();
    let busy = StubPartition::holding(vec![event(tenant, 1)]);
    let other = StubPartition::holding(vec![event(tenant, 2)]);
    let mut harness = open(
        vec![(0, StubPartition::empty()), (1, busy), (2, other)],
        tenant,
    );

    let frames = drain(&mut harness.session, 4).await;

    // The empty partition is skipped rather than taking a turn, so both busy
    // ones are served.
    assert_eq!(kinds(&frames), vec!["topology", "event", "event"]);
}

fn assignment(partition: i32) -> Assignment {
    Assignment {
        topic: gts(TOPIC),
        partition,
        offset: Sequence::assigned(0),
        last_examined: Sequence::assigned(0),
    }
}

/// A partition whose reader always claims readiness but whose reads never yield.
///
/// Not contrived: `has_data` is optimistic by design - the accounted frontier
/// being ahead of a reader does not mean the gap in front of *that* reader has
/// been filled - so a reader in a gap is perpetually "ready" while every read
/// reports the position unanswerable.
struct LyingPartition {
    reads: AtomicI64,
}

impl PartitionReader for LyingPartition {
    fn has_data(&self) -> bool {
        true
    }

    fn read(&self, _limit: ReadLimit) -> PartitionRead {
        self.reads.fetch_add(1, Ordering::Relaxed);
        PartitionRead::Unknown
    }

    fn seek(&self, _offset: Sequence) {}

    fn report_scanning(&self, _scanning: bool) {}
}

#[tokio::test]
async fn a_perpetually_ready_partition_that_never_yields_does_not_spin() {
    let tenant = Uuid::new_v4();
    let lying = Arc::new(LyingPartition {
        reads: AtomicI64::new(0),
    });
    let leases = Arc::new(InProcessStreamLeases::new());
    let lease = leases.acquire(Uuid::new_v4()).expect("lease is free");
    let filter: Arc<dyn EventFilter> = Arc::new(
        InterestFilter::compile(&[crate::domain::model::Interest {
            topic: gts(TOPIC),
            tenant_id: tenant,
            depth: TenantTraversalDepth::CurrentTenant,
            barrier_mode: BarrierMode::Respect,
            types: vec![CREATED.to_owned()],
            filter: None,
        }])
        .expect("compiles"),
    );
    let (generations, membership) = membership(8);
    let mut session = StreamSession::open(SessionOpening {
        read_set: ReadSet::seed(vec![PartitionSlot::new(
            key(0),
            Arc::clone(&lying) as Arc<dyn PartitionReader>,
        )]),
        filter,
        progress: ProgressConfig::default(),
        heartbeat_interval: Duration::from_secs(5),
        limit: ReadLimit::new(MaxEvents(256), MaxBytes(1024 * 1024)),
        topology_version: 1,
        ready: Arc::new(Notify::new()),
        started_at: Instant::now(),
        now: Arc::new(Utc::now),
        unanswerable_tolerance: Duration::from_secs(30),
        lease,
        generations,
        membership,
    });

    let baseline = session.next_frame().await;
    assert!(matches!(baseline, Some(Frame::Topology { .. })));

    // Real time, and the heartbeat is five seconds away, so a session that
    // parks makes at most a handful of reads before this expires. A spinning one
    // makes thousands.
    // Expected to expire: the point is that the session parked, not that it
    // produced anything.
    let parked = tokio::time::timeout(Duration::from_millis(50), session.next_frame()).await;
    assert!(
        parked.is_err(),
        "nothing is owed yet, so it should still be waiting"
    );

    let reads = lying.reads.load(Ordering::Relaxed);
    assert!(
        reads < 10,
        "the session issued {reads} reads in 50ms - it is spinning on an \
         optimistically-ready reader instead of parking"
    );
}

#[tokio::test]
async fn a_session_idle_across_many_topics_parks_on_one_waker() {
    let tenant = Uuid::new_v4();
    let leases = Arc::new(InProcessStreamLeases::new());
    let lease = leases.acquire(Uuid::new_v4()).expect("lease is free");

    // Four topics of eight partitions, all silent - the shape a consumer group
    // subscribing broadly actually has.
    let mut slots = Vec::new();
    let mut interests = Vec::new();
    let mut counters = Vec::new();
    for topic_index in 0..4 {
        let topic = gts(&format!(
            "gts.cf.core.events.topic.v1~x.eb.t{topic_index}.acme.v1"
        ));
        interests.push(crate::domain::model::Interest {
            topic: topic.clone(),
            tenant_id: tenant,
            depth: TenantTraversalDepth::CurrentTenant,
            barrier_mode: BarrierMode::Respect,
            types: vec![CREATED.to_owned()],
            filter: None,
        });
        for partition in 0..8 {
            let quiet = Arc::new(LyingPartition {
                reads: AtomicI64::new(0),
            });
            counters.push(Arc::clone(&quiet));
            slots.push(PartitionSlot::new(
                PartitionKey::new(topic.clone(), partition),
                Arc::clone(&quiet) as Arc<dyn PartitionReader>,
            ));
        }
    }

    let filter: Arc<dyn EventFilter> =
        Arc::new(InterestFilter::compile(&interests).expect("compiles"));
    let (generations, membership) = membership(8);
    let mut session = StreamSession::open(SessionOpening {
        read_set: ReadSet::seed(slots),
        filter,
        progress: ProgressConfig::default(),
        heartbeat_interval: Duration::from_secs(5),
        limit: ReadLimit::new(MaxEvents(256), MaxBytes(1024 * 1024)),
        topology_version: 1,
        ready: Arc::new(Notify::new()),
        started_at: Instant::now(),
        now: Arc::new(Utc::now),
        unanswerable_tolerance: Duration::from_secs(30),
        lease,
        generations,
        membership,
    });

    let baseline = session.next_frame().await;
    assert!(matches!(baseline, Some(Frame::Topology { .. })));

    let parked = tokio::time::timeout(Duration::from_millis(50), session.next_frame()).await;
    assert!(
        parked.is_err(),
        "an idle session owes nothing for five seconds"
    );

    // Every reader claims readiness, so each partition is asked exactly once
    // before the round is found fruitless and the session parks on its single
    // shared waker. Thirty-two partitions therefore cost thirty-two reads, not
    // thirty-two futures and not an unbounded loop.
    let total: i64 = counters
        .iter()
        .map(|quiet| quiet.reads.load(Ordering::Relaxed))
        .sum();
    assert!(
        total <= 32,
        "thirty-two idle partitions issued {total} reads in 50ms - the session \
         is not parking after a fruitless round"
    );
}

#[tokio::test]
async fn a_quiet_tail_never_tears_the_stream_down() {
    tokio::time::pause();
    let tenant = Uuid::new_v4();
    let mut harness = open(vec![(0, StubPartition::empty())], tenant);
    let _ = harness.session.next_frame().await;

    // An empty partition reports `NothingNew`, which is a healthy idle stream.
    // Hours of it must produce heartbeats, never a teardown - conflating a quiet
    // tail with an unanswerable position would kill every idle consumer.
    for _ in 0..20 {
        tokio::time::advance(Duration::from_secs(6)).await;
        let frame = harness.session.next_frame().await;
        assert!(
            matches!(frame, Some(Frame::Heartbeat { .. })),
            "expected a heartbeat, got {frame:?}"
        );
    }
    assert_eq!(harness.session.state(), SessionState::Streaming);
}

#[tokio::test]
async fn a_sustained_unanswerable_position_tears_the_stream_down() {
    tokio::time::pause();
    let tenant = Uuid::new_v4();
    let lying = Arc::new(LyingPartition {
        reads: AtomicI64::new(0),
    });
    let leases = Arc::new(InProcessStreamLeases::new());
    let lease = leases.acquire(Uuid::new_v4()).expect("lease is free");
    let filter: Arc<dyn EventFilter> = Arc::new(
        InterestFilter::compile(&[crate::domain::model::Interest {
            topic: gts(TOPIC),
            tenant_id: tenant,
            depth: TenantTraversalDepth::CurrentTenant,
            barrier_mode: BarrierMode::Respect,
            types: vec![CREATED.to_owned()],
            filter: None,
        }])
        .expect("compiles"),
    );
    let (generations, membership) = membership(8);
    let mut session = StreamSession::open(SessionOpening {
        read_set: ReadSet::seed(vec![PartitionSlot::new(
            key(0),
            Arc::clone(&lying) as Arc<dyn PartitionReader>,
        )]),
        filter,
        progress: ProgressConfig::default(),
        heartbeat_interval: Duration::from_secs(5),
        limit: ReadLimit::new(MaxEvents(256), MaxBytes(1024 * 1024)),
        topology_version: 1,
        ready: Arc::new(Notify::new()),
        started_at: Instant::now(),
        now: Arc::new(Utc::now),
        unanswerable_tolerance: Duration::from_secs(30),
        lease,
        generations,
        membership,
    });
    let _ = session.next_frame().await;

    // The position never becomes answerable. Heartbeats carry on while the
    // tolerance runs, and then the stream ends with a reason the consumer can
    // act on rather than an apparently-alive stream that delivers nothing.
    let mut frames = Vec::new();
    for _ in 0..12 {
        tokio::time::advance(Duration::from_secs(6)).await;
        if let Some(frame) = session.next_frame().await {
            let terminal = matches!(frame, Frame::Control { .. });
            frames.push(frame);
            if terminal {
                break;
            }
        }
    }

    match frames.last() {
        Some(Frame::Control { reason, .. }) => {
            assert_eq!(*reason, Some(CloseReason::Teardown));
        }
        other => panic!("expected a teardown terminal frame, got {other:?}"),
    }
    assert!(
        frames.len() > 1,
        "it should have heartbeated while the tolerance ran, not given up at once"
    );
    assert_eq!(session.state(), SessionState::Closed);
}

#[tokio::test]
async fn a_version_only_delta_reports_the_version_and_continues() {
    let tenant = Uuid::new_v4();
    let mut harness = open(vec![(0, StubPartition::empty())], tenant);
    let _ = harness.session.next_frame().await;

    let before = Generation::new(7, vec![assignment(0)]);
    let after = Generation::new(9, vec![assignment(0)]);
    harness
        .session
        .apply(&AssignmentDelta::classify(&before, &after));

    match harness.session.next_frame().await {
        Some(Frame::Topology {
            topology_version, ..
        }) => assert_eq!(topology_version, 9),
        other => panic!("expected a topology frame, got {other:?}"),
    }
    assert_eq!(harness.session.state(), SessionState::Streaming);
}

#[tokio::test]
async fn a_loss_and_a_gain_together_terminate() {
    let tenant = Uuid::new_v4();
    let mut harness = open(
        vec![(0, StubPartition::empty()), (1, StubPartition::empty())],
        tenant,
    );
    let _ = harness.session.next_frame().await;

    // Partition 0 goes, 2 arrives. The loss alone would continue; the gain
    // cannot, because a gained partition has no cursor here.
    let before = Generation::new(7, vec![assignment(0), assignment(1)]);
    let after = Generation::new(8, vec![assignment(1), assignment(2)]);
    harness
        .session
        .apply(&AssignmentDelta::classify(&before, &after));

    match harness.session.next_frame().await {
        Some(Frame::Control { reason, .. }) => {
            assert_eq!(reason, Some(CloseReason::Rebalanced));
        }
        other => panic!("expected a terminal frame, got {other:?}"),
    }
}

#[tokio::test]
async fn dropping_an_unpolled_session_releases_its_lease() {
    let leases = Arc::new(InProcessStreamLeases::new());
    let subscription = Uuid::new_v4();
    let lease = leases.acquire(subscription).expect("lease is free");
    let filter: Arc<dyn EventFilter> = Arc::new(InterestFilter::compile(&[]).expect("compiles"));

    let (generations, membership) = membership(8);
    let session = StreamSession::open(SessionOpening {
        read_set: ReadSet::seed(Vec::new()),
        filter,
        progress: ProgressConfig::default(),
        heartbeat_interval: Duration::from_secs(5),
        limit: ReadLimit::new(MaxEvents(1), MaxBytes(1)),
        topology_version: 1,
        ready: Arc::new(Notify::new()),
        started_at: Instant::now(),
        now: Arc::new(Utc::now),
        unanswerable_tolerance: Duration::from_secs(30),
        lease,
        generations,
        membership,
    });
    assert!(leases.is_held(subscription));

    // Never polled. A client that opens a stream and vanishes must not leave the
    // subscription unable to stream again, and release is ownership rather than
    // a guard somebody has to run.
    drop(session);

    assert!(!leases.is_held(subscription));
}

#[tokio::test]
async fn the_frame_stream_yields_the_same_sequence_as_the_session() {
    use tokio_stream::StreamExt;

    let tenant = Uuid::new_v4();
    let held = StubPartition::holding(vec![event(tenant, 1), event(tenant, 2)]);
    let harness = open(vec![(0, held)], tenant);
    let mut stream = crate::domain::streaming::session::FrameStream::new(harness.session);

    let mut kinds_seen = Vec::new();
    for _ in 0..3 {
        match tokio::time::timeout(Duration::from_millis(200), stream.next()).await {
            Ok(Some(frame)) => kinds_seen.push(kinds(&[frame]).remove(0)),
            Ok(None) | Err(_) => break,
        }
    }

    assert_eq!(kinds_seen, vec!["topology", "event", "event"]);
}

#[tokio::test]
async fn the_frame_stream_ends_when_the_session_closes() {
    use tokio_stream::StreamExt;

    let tenant = Uuid::new_v4();
    let mut harness = open(vec![(0, StubPartition::empty())], tenant);
    let _ = harness.session.next_frame().await;
    harness.session.tear_down();

    let mut stream = crate::domain::streaming::session::FrameStream::new(harness.session);

    let terminal = stream.next().await;
    assert!(matches!(terminal, Some(Frame::Control { .. })));
    assert!(stream.next().await.is_none(), "and stays ended");
}

/// A quiet session must park between frames, not spin.
///
/// The rhythm no other test here uses: **progress overdue, heartbeat not yet
/// due**. Every other idle test advances past the heartbeat, so the heartbeat
/// branch returns first and the park below it is never reached - which is
/// exactly why the defect this guards survived.
///
/// Falsify by reverting the unconditional `record_emitted` in `advance`: the
/// progress timer then stays permanently overdue, the deadline handed to the
/// park is already expired, and the probe count runs to millions.
#[tokio::test]
async fn a_quiet_session_parks_between_frames_rather_than_spinning() {
    let tenant = Uuid::new_v4();
    let held = StubPartition::empty();
    let leases = Arc::new(InProcessStreamLeases::new());
    let lease = leases.acquire(Uuid::new_v4()).expect("lease is free");
    let (generations, membership) = membership(8);

    let filter: Arc<dyn EventFilter> = Arc::new(
        InterestFilter::compile(&[crate::domain::model::Interest {
            topic: gts(TOPIC),
            tenant_id: tenant,
            depth: TenantTraversalDepth::CurrentTenant,
            barrier_mode: BarrierMode::Respect,
            types: vec![CREATED.to_owned()],
            filter: None,
        }])
        .expect("compiles"),
    );

    let now: NowFn = Arc::new(Utc::now);
    let mut session = StreamSession::open(SessionOpening {
        read_set: ReadSet::seed(vec![PartitionSlot::new(
            key(0),
            Arc::clone(&held) as Arc<dyn PartitionReader>,
        )]),
        filter,
        // Progress falls due almost immediately and has nothing to report,
        // because an empty partition never drifts. The heartbeat is the only
        // frame that can legitimately arrive, and it is what bounds the test.
        progress: ProgressConfig {
            drift_threshold: 1000,
            min_interval: Duration::from_millis(10),
        },
        heartbeat_interval: Duration::from_millis(300),
        limit: ReadLimit::new(MaxEvents(256), MaxBytes(1024 * 1024)),
        topology_version: 7,
        ready: Arc::new(Notify::new()),
        started_at: Instant::now(),
        now,
        unanswerable_tolerance: Duration::from_secs(300),
        lease,
        generations,
        membership,
    });

    // Real time, not paused: a spinning task never yields to a paused clock, so
    // the failure would be a hang rather than an assertion.
    let first = session.next_frame().await;
    assert!(
        matches!(first, Some(Frame::Topology { .. })),
        "expected the open-time baseline, got {first:?}"
    );
    let after_baseline = held.probes.load(Ordering::Relaxed);

    let second = tokio::time::timeout(Duration::from_secs(5), session.next_frame())
        .await
        .expect("a heartbeat must arrive within 5s");
    assert!(
        matches!(second, Some(Frame::Heartbeat { .. })),
        "expected a heartbeat, got {second:?}"
    );

    // Across ~300ms of idling the session should probe a handful of times - once
    // per wake. A spin does it as fast as the CPU allows.
    let probes = held.probes.load(Ordering::Relaxed) - after_baseline;
    assert!(
        probes < 1000,
        "a quiet session probed readiness {probes} times between two frames; \
         it is spinning rather than parking"
    );
}
