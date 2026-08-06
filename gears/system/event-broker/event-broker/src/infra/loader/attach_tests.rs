//! Streaming wired to real caches, with no stub in the path.
//!
//! Every other session test substitutes a stub for the cache, which proves the
//! session's logic and nothing about the join between them. These drive a real
//! `TopicManager`, real `PartitionCache`s and real `ReaderHandle`s, and absorb
//! through the loader's own entry point - so a mismatch between the two halves
//! fails here rather than at the first real request.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::Instant;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::model::{
    Assignment, BarrierMode, Cursor, Event, Interest, Sequence, TenantTraversalDepth,
};
use crate::domain::streaming::filter::{EventFilter, InterestFilter};
use crate::domain::streaming::frames::{ControlCode, Frame};
use crate::domain::streaming::lease::{InProcessStreamLeases, StreamLeases};
use crate::domain::streaming::progress::ProgressConfig;
use crate::domain::streaming::read::{MaxBytes, MaxEvents, ReadLimit};
use crate::domain::streaming::read_set::ReadSet;
use crate::domain::streaming::session::{SessionOpening, StreamSession};
use crate::domain::streaming::source::PartitionKey;
use crate::domain::streaming::time::NowFn;
use crate::infra::partition_cache::cache::AbsorbedFetch;
use crate::infra::partition_cache::segment::Segment;

use super::attach::{AttachRequest, attach_readers};
use super::topics::{TopicManager, TopicPolicy};

const TOPIC: &str = "gts.cf.core.events.topic.v1~x.eb.orders.acme.v1";
const CREATED: &str = "gts.cf.core.events.event.v1~x.eb.o.created.v1~";

fn gts(id: &str) -> GtsInstanceId {
    GtsInstanceId::try_new(id).expect("static gts id is valid")
}

fn event(sequence: i64, tenant: Uuid) -> Event {
    let sequence = Sequence::assigned(sequence);
    Event {
        id: Uuid::nil(),
        r#type: crate::test_support::event_type_id(CREATED),
        topic: gts(TOPIC),
        tenant_id: tenant,
        source: "test".to_owned(),
        subject: "test".to_owned(),
        subject_type: "test".to_owned(),
        occurred_at: Utc::now(),
        trace_parent: None,
        data: json!({ "n": sequence }),
        meta: None,
        partition: Some(0),
        sequence: Some(sequence),
        sequence_time: None,
    }
}

/// What the loader does when a fetch comes back, through the same entry point.
fn absorb(manager: &TopicManager, partition: i32, from: i64, through: i64, tenant: Uuid) {
    let key = PartitionKey::new(gts(TOPIC), partition);
    let segment = Segment::builder()
        .from(Sequence::assigned(from))
        .through(Sequence::assigned(through))
        .events(
            (from..=through)
                .map(|sequence| event(sequence, tenant))
                .collect(),
        )
        .build();
    manager
        .attach(&key)
        .cache()
        .absorb(AbsorbedFetch::builder(segment).build());
}

/// A real coordinator with one member, so the session gets a real generation
/// watch and a real membership handle. In-memory, no I/O - the same reason the
/// lease uses the production `InProcessStreamLeases`.
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

struct Wired {
    session: StreamSession,
    manager: Arc<TopicManager>,
    ready: Arc<Notify>,
    _leases: Arc<InProcessStreamLeases>,
}

fn wire(partitions: &[i32], cursors: &[Cursor], tenant: Uuid) -> Wired {
    let manager = Arc::new(TopicManager::new(TopicPolicy::default()));
    let ready = Arc::new(Notify::new());
    let leases = Arc::new(InProcessStreamLeases::new());
    let lease = leases.acquire(Uuid::new_v4()).expect("lease is free");

    let assigned: Vec<Assignment> = partitions
        .iter()
        .map(|partition| Assignment {
            topic: gts(TOPIC),
            partition: *partition,
            offset: Sequence::assigned(0),
            last_examined: Sequence::assigned(0),
        })
        .collect();

    let slots = attach_readers(&AttachRequest {
        topics: &manager,
        assigned: &assigned,
        cursors,
        ready: &ready,
    });

    let filter: Arc<dyn EventFilter> = Arc::new(
        InterestFilter::compile(&[Interest {
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
        topology_version: 3,
        ready: Arc::clone(&ready),
        started_at: Instant::now(),
        now,
        unanswerable_tolerance: Duration::from_secs(30),
        lease,
        generations,
        membership,
    });

    Wired {
        session,
        manager,
        ready,
        _leases: leases,
    }
}

fn sequences(frames: &[Frame]) -> Vec<i64> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Event(event) => event.sequence,
            _ => None,
        })
        .map(Sequence::as_i64)
        .collect()
}

async fn collect(session: &mut StreamSession, limit: usize) -> Vec<Frame> {
    let mut frames = Vec::new();
    for _ in 0..limit {
        match tokio::time::timeout(Duration::from_millis(200), session.next_frame()).await {
            Ok(Some(frame)) => frames.push(frame),
            Ok(None) | Err(_) => break,
        }
    }
    frames
}

/// The join, end to end: the loader absorbs into a real cache and a real
/// session delivers what it absorbed.
#[tokio::test]
async fn an_absorbed_fetch_is_delivered_by_a_session_reading_the_real_cache() {
    let tenant = Uuid::new_v4();
    let mut wired = wire(&[0], &[], tenant);

    absorb(&wired.manager, 0, 1, 5, tenant);
    wired.ready.notify_one();

    let frames = collect(&mut wired.session, 8).await;

    assert!(
        matches!(frames.first(), Some(Frame::Topology { .. })),
        "the first frame is the open-time baseline, got {:?}",
        frames.first()
    );
    assert_eq!(sequences(&frames), vec![1, 2, 3, 4, 5]);
}

/// A cursor is where a session starts, and the partition's own `Assignment`
/// offsets are deliberately not consulted - they are the SDK's fields.
#[tokio::test]
async fn a_persisted_cursor_is_where_delivery_begins() {
    let tenant = Uuid::new_v4();
    let cursors = vec![Cursor {
        topic: gts(TOPIC),
        // A consumer-group instance id is a UUID, not a dotted name.
        consumer_group: gts(
            "gts.cf.core.events.consumer_group.v1~02943530-10da-4624-a3ae-b998c425847f",
        ),
        partition: 0,
        offset: Sequence::assigned(3),
    }];
    let mut wired = wire(&[0], &cursors, tenant);

    absorb(&wired.manager, 0, 1, 6, tenant);
    wired.ready.notify_one();

    let frames = collect(&mut wired.session, 8).await;

    // 1..=3 are below the cursor and must not be re-delivered.
    assert_eq!(sequences(&frames), vec![4, 5, 6]);
}

/// One waker for the whole assignment: an absorb on any partition wakes the
/// session, and it then finds the ready one by checking rather than by awaiting
/// per partition.
#[tokio::test]
async fn one_absorb_on_one_partition_serves_a_session_holding_several() {
    let tenant = Uuid::new_v4();
    let mut wired = wire(&[0, 1, 2], &[], tenant);

    absorb(&wired.manager, 2, 1, 3, tenant);
    wired.ready.notify_one();

    let frames = collect(&mut wired.session, 8).await;

    assert_eq!(sequences(&frames), vec![1, 2, 3]);
}

/// Attaching is what creates a partition's cache - nothing exists until a
/// session asks, and what a session holds is not retired underneath it.
#[tokio::test]
async fn attaching_creates_the_partitions_and_holds_them_against_retirement() {
    let tenant = Uuid::new_v4();
    let manager = Arc::new(TopicManager::new(TopicPolicy::default()));
    assert_eq!(manager.live().len(), 0, "nothing exists before an attach");

    let ready = Arc::new(Notify::new());
    let assigned: Vec<Assignment> = (0..4)
        .map(|partition| Assignment {
            topic: gts(TOPIC),
            partition,
            offset: Sequence::assigned(0),
            last_examined: Sequence::assigned(0),
        })
        .collect();

    let slots = attach_readers(&AttachRequest {
        topics: &manager,
        assigned: &assigned,
        cursors: &[],
        ready: &ready,
    });
    assert_eq!(manager.live().len(), 4);

    // Long past any idle threshold, but the session still holds them.
    assert_eq!(
        manager.retire_idle(1_000_000, 1),
        0,
        "a partition a session holds must not be retired - a later attach would \
         build a second cache for the same key"
    );

    drop(slots);
    assert_eq!(
        manager.retire_idle(1_000_000, 1),
        4,
        "once the session's readers are gone the partitions are retirable"
    );
    let _ = tenant;
}

/// The unwired half, stated as a test rather than left implicit: with nothing
/// driving the loader, a session reading a partition nothing has absorbed into
/// gets no events. This is the wiring line, and it is not closed yet.
#[tokio::test]
async fn a_session_with_no_loader_behind_it_delivers_nothing() {
    let tenant = Uuid::new_v4();
    let mut wired = wire(&[0], &[], tenant);

    let frames = collect(&mut wired.session, 4).await;

    assert!(sequences(&frames).is_empty());
    assert!(
        frames.iter().all(|frame| matches!(
            frame,
            Frame::Topology { .. }
                | Frame::Heartbeat { .. }
                | Frame::Control {
                    code: ControlCode::Progress,
                    ..
                }
        )),
        "expected only baseline and cadence frames, got {frames:?}"
    );
}
