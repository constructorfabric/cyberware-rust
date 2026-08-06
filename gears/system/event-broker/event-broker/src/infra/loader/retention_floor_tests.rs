//! What a reader parked below a retention floor sees.
//!
//! Retention removes a prefix, so a partition's floor moves up under readers
//! that may be standing below it. The cache already distinguishes a position
//! that is **proven absent** from one that is **unknown**, and a fetch is what
//! establishes the difference: a read aimed at an offset accounts for the whole
//! span from that offset up to the highest sequence it returned, so sequences
//! retention took fall inside an accounted span rather than leaving a hole a
//! reader must wait on.
//!
//! That makes the floor a derived fact rather than one anything has to publish.
//! Nothing here tells the cache where the floor is; the loader's ordinary fetch
//! does, and these tests are what hold that property in place.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use chrono::Utc;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::model::{
    Assignment, BarrierMode, Event, Interest, Sequence, TenantTraversalDepth,
};
use crate::domain::streaming::filter::{EventFilter, InterestFilter};
use crate::domain::streaming::frames::Frame;
use crate::domain::streaming::lease::{InProcessStreamLeases, StreamLeases};
use crate::domain::streaming::progress::ProgressConfig;
use crate::domain::streaming::read::{MaxBytes, MaxEvents, PartitionRead, ReadLimit};
use crate::domain::streaming::read_set::ReadSet;
use crate::domain::streaming::session::{SessionOpening, StreamSession};
use crate::domain::streaming::source::PartitionKey;
use crate::domain::streaming::time::NowFn;

use super::attach::{AttachRequest, attach_readers};
use super::scheduler::{DemandScheduler, SchedulerPolicy};
use super::shard::ShardLoader;
use super::source::{EventSource, SourceError};
use super::topics::{TopicManager, TopicPolicy};

const TOPIC: &str = "gts.cf.core.events.topic.v1~x.eb.orders.acme.v1";
const CREATED: &str = "gts.cf.core.events.event.v1~x.eb.o.created.v1~";
const GROUP: &str = "gts.cf.core.events.consumer_group.v1~02943530-10da-4624-a3ae-b998c425847f";

/// The sequences retention left in place, written out rather than described as
/// a span. A count taken from two sequence numbers is the mistake these tests
/// exist to rule out, so nothing here derives one that way - `SURVIVING.len()`
/// counts the events.
const SURVIVING: [Sequence; 5] = [
    Sequence::assigned(6),
    Sequence::assigned(7),
    Sequence::assigned(8),
    Sequence::assigned(9),
    Sequence::assigned(10),
];

/// The lowest sequence still stored: retention took everything below it.
const FLOOR: Sequence = SURVIVING[0];
/// The highest sequence stored.
const THROUGH: Sequence = SURVIVING[SURVIVING.len() - 1];

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

/// A partition retention has already trimmed: sequences below [`FLOOR`] are
/// gone, and a read aimed below the floor skips straight to the oldest
/// survivor, exactly as the `SQLite` backend's own `read` does after a pass.
struct Trimmed {
    tenant: Uuid,
    reads: AtomicI64,
    /// The exclusive offsets fetches were aimed at, in order. What matters is
    /// not how many fetches happen - the tail poller issues one whenever a
    /// reader is caught up - but whether any of them is still aimed below the
    /// floor, which is what a reader stuck on an unknown position produces.
    aimed_at: std::sync::Mutex<Vec<Sequence>>,
}

impl EventSource for Trimmed {
    async fn read(
        &self,
        _key: &PartitionKey,
        after: Sequence,
        max_events: usize,
    ) -> Result<Vec<Event>, SourceError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.aimed_at
            .lock()
            .expect("no panics under this lock")
            .push(after);
        // A window is arithmetic over counts; only each event's position is a
        // `Sequence`, so the span is computed in integers and crossed per item.
        let from = after.as_i64().saturating_add(1).max(FLOOR.as_i64());
        if from > THROUGH.as_i64() {
            return Ok(Vec::new());
        }
        let last = from
            .saturating_add(i64::try_from(max_events).unwrap_or(i64::MAX))
            .saturating_sub(1)
            .min(THROUGH.as_i64());
        Ok((from..=last).map(|s| event(s, self.tenant)).collect())
    }
}

fn parked_reader(
    topics: &TopicManager,
    ready: &Arc<Notify>,
) -> Vec<crate::domain::streaming::read_set::PartitionSlot> {
    attach_readers(&AttachRequest {
        topics,
        assigned: &[Assignment {
            topic: gts(TOPIC),
            partition: 0,
            offset: Sequence::assigned(0),
            last_examined: Sequence::assigned(0),
        }],
        // Offset 0: below the floor, which is the whole point.
        cursors: &[],
        ready,
    })
}

/// The floor reaches the cache as an accounted span, so a reader below it is
/// served the oldest survivor rather than parked.
#[tokio::test]
async fn a_fetch_from_below_the_floor_accounts_for_the_removed_prefix() {
    let tenant = Uuid::new_v4();
    let topics = Arc::new(TopicManager::new(TopicPolicy::default()));
    let source = Arc::new(Trimmed {
        tenant,
        reads: AtomicI64::new(0),
        aimed_at: std::sync::Mutex::new(Vec::new()),
    });
    let scheduler = DemandScheduler::new(
        Arc::clone(&source),
        Arc::clone(&topics),
        SchedulerPolicy::with_pool(1).build(),
    );

    let ready = Arc::new(Notify::new());
    let _slots = parked_reader(&topics, &ready);

    // One round, driven. Nothing here absorbs, so whatever the cache knows
    // afterwards came from the loader's own fetch.
    scheduler.run_round().await;

    let partition = topics.attach(&PartitionKey::new(gts(TOPIC), 0));
    match partition
        .cache()
        .read_from(Sequence::assigned(0), ReadLimit::unbounded())
    {
        PartitionRead::Hit {
            events,
            accounted_through,
        } => {
            assert_eq!(
                events.iter().filter_map(|e| e.sequence).collect::<Vec<_>>(),
                SURVIVING.to_vec(),
                "a read from below the floor is served the oldest survivor onwards"
            );
            assert_eq!(
                accounted_through, THROUGH,
                "the accounted span covers the removed prefix, so the reader may \
                 step over it"
            );
        }
        other => panic!(
            "a position retention removed must read as proven absent, not as a \
             gap to wait on; got {other:?}"
        ),
    }
}

/// The failure mode named in the design, asserted not to happen: if removed
/// positions read as *unknown*, the loader fetches for them forever and the
/// reader below the floor never advances.
#[tokio::test]
async fn the_loader_does_not_fetch_forever_for_removed_positions() {
    let tenant = Uuid::new_v4();
    let topics = Arc::new(TopicManager::new(TopicPolicy::default()));
    let source = Arc::new(Trimmed {
        tenant,
        reads: AtomicI64::new(0),
        aimed_at: std::sync::Mutex::new(Vec::new()),
    });
    let scheduler = DemandScheduler::new(
        Arc::clone(&source),
        Arc::clone(&topics),
        SchedulerPolicy::with_pool(1).build(),
    );

    let ready = Arc::new(Notify::new());
    let slots = parked_reader(&topics, &ready);
    let partition = topics.attach(&PartitionKey::new(gts(TOPIC), 0));

    // The first round fills the partition from the floor up.
    scheduler.run_round().await;
    assert_eq!(
        *source.aimed_at.lock().expect("no panics under this lock"),
        vec![Sequence::NONE],
        "the first fetch is aimed at the reader's position, below the floor"
    );

    // Advance the reader the way a session does: read, which moves its frontier
    // to whatever the read accounted for.
    let read = slots
        .first()
        .expect("one partition was assigned")
        .reader()
        .read(ReadLimit::unbounded());
    assert!(
        matches!(read, PartitionRead::Hit { .. }),
        "the parked reader was served rather than left waiting: {read:?}"
    );

    for _ in 0..8 {
        scheduler.run_round().await;
    }

    let aimed_at = source.aimed_at.lock().expect("no panics under this lock");
    let (first, rest) = aimed_at.split_first().expect("at least the first fetch");
    assert_eq!(*first, Sequence::assigned(0));
    assert!(
        rest.iter().all(|after| *after == THROUGH),
        "every fetch after the first is aimed at the tail; one still aimed below \
         the floor would mean the reader never advanced past what retention \
         removed, which is what treating a removed position as unknown looks \
         like: {aimed_at:?}"
    );
    assert_eq!(
        partition.cache().stats().resident().events(),
        SURVIVING.len() as u64,
        "only the surviving events are resident"
    );
}

fn interest_filter(tenant: Uuid) -> Arc<dyn EventFilter> {
    Arc::new(
        InterestFilter::compile(&[Interest {
            topic: gts(TOPIC),
            tenant_id: tenant,
            depth: TenantTraversalDepth::CurrentTenant,
            barrier_mode: BarrierMode::Respect,
            types: vec![CREATED.to_owned()],
            filter: None,
        }])
        .expect("compiles"),
    )
}

fn membership(
    partitions: i32,
) -> (
    tokio::sync::watch::Receiver<crate::domain::streaming::assignment::Generation>,
    crate::domain::consumer_group_coordinator::MembershipHandle,
) {
    use crate::domain::consumer_group_coordinator::{ConsumerGroupCoordinator, TopicInterest};

    let groups = Arc::new(ConsumerGroupCoordinator::new());
    let group = gts(GROUP);
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

/// Every frame a session emits before it has delivered `want` events, so a test
/// can assert on what was *not* sent as well as what was.
async fn drain_frames(session: &mut StreamSession, want: usize) -> Vec<Frame> {
    let mut frames = Vec::new();
    let mut events = 0;
    for _ in 0..(want * 8 + 64) {
        match tokio::time::timeout(Duration::from_millis(500), session.next_frame()).await {
            Ok(Some(frame)) => {
                if matches!(frame, Frame::Event(_)) {
                    events += 1;
                }
                frames.push(frame);
                if events >= want {
                    return frames;
                }
            }
            Ok(None) | Err(_) => return frames,
        }
    }
    frames
}

/// A consumer is owed the oldest survivor and no explanation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_consumer_reading_from_a_removed_position_is_served_the_oldest_survivor() {
    let tenant = Uuid::new_v4();
    let topics = Arc::new(TopicManager::new(TopicPolicy::default()));
    let source = Arc::new(Trimmed {
        tenant,
        reads: AtomicI64::new(0),
        aimed_at: std::sync::Mutex::new(Vec::new()),
    });
    let scheduler = Arc::new(DemandScheduler::new(
        Arc::clone(&source),
        Arc::clone(&topics),
        SchedulerPolicy::with_pool(2).build(),
    ));

    let ready = Arc::new(Notify::new());
    let leases = Arc::new(InProcessStreamLeases::new());
    let lease = leases.acquire(Uuid::new_v4()).expect("lease is free");
    let slots = parked_reader(&topics, &ready);

    let shutdown = CancellationToken::new();
    let loader = ShardLoader::new(
        Arc::clone(&scheduler),
        Arc::clone(&topics),
        Duration::from_millis(5),
    )
    .spawn(shutdown.clone());

    let now: NowFn = Arc::new(Utc::now);
    let (generations, membership) = membership(8);
    let mut session = StreamSession::open(SessionOpening {
        read_set: ReadSet::seed(slots),
        filter: interest_filter(tenant),
        progress: ProgressConfig::default(),
        heartbeat_interval: Duration::from_millis(50),
        limit: ReadLimit::new(MaxEvents(8), MaxBytes(1024 * 1024)),
        topology_version: 1,
        ready: Arc::clone(&ready),
        started_at: Instant::now(),
        now,
        unanswerable_tolerance: Duration::from_secs(30),
        lease,
        generations,
        membership,
    });

    let frames = drain_frames(&mut session, SURVIVING.len()).await;

    shutdown.cancel();
    loader.await.expect("loader task did not panic");

    let delivered: Vec<Sequence> = frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Event(event) => event.sequence,
            _ => None,
        })
        .collect();
    assert_eq!(
        delivered,
        SURVIVING.to_vec(),
        "delivery begins at the oldest surviving event, not at the position asked \
         for and not with an error"
    );

    // The broker owes a consumer no account of what retention took. A stream
    // opens with a topology frame carrying the positions it was opened at, and
    // may heartbeat; anything beyond that - a control frame, a close reason -
    // would be narrating the gap between the position asked for and the one
    // served, and that is a promise about sequence continuity the broker does
    // not make.
    let unexpected: Vec<&Frame> = frames
        .iter()
        .filter(|frame| {
            !matches!(
                frame,
                Frame::Event(_) | Frame::Heartbeat { .. } | Frame::Topology { .. }
            )
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "no frame may account for what retention removed; got {unexpected:?}"
    );
}
