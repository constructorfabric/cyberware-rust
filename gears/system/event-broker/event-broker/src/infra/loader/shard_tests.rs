//! The loader driving a real cache under a real session: the wiring line.
//!
//! `attach_tests` proves a session reads what something else absorbed. This
//! proves the loader is that something else - nothing here calls `absorb`, so
//! delivery only happens if the shard task fetched, absorbed and woke the
//! session on its own.

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
use crate::domain::streaming::read::{MaxBytes, MaxEvents, ReadLimit};
use crate::domain::streaming::read_set::ReadSet;
use crate::domain::streaming::session::{SessionOpening, StreamSession};
use crate::domain::streaming::source::PartitionKey;
use crate::domain::streaming::time::NowFn;

use crate::infra::partition_cache::cache::AbsorbedFetch;
use crate::infra::partition_cache::segment::Segment;

use super::attach::{AttachRequest, attach_readers};
use super::scheduler::{DemandScheduler, SchedulerPolicy};
use super::shard::ShardLoader;
use super::source::{EventSource, SourceError};
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

/// A backend holding a fixed number of events per partition. Counts reads so a
/// test can tell "the loader fetched" from "the loader spun".
struct Stored {
    through: Sequence,
    tenant: Uuid,
    reads: AtomicI64,
}

impl EventSource for Stored {
    async fn read(
        &self,
        _key: &PartitionKey,
        after: Sequence,
        max_events: usize,
    ) -> Result<Vec<Event>, SourceError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        // A window is arithmetic over counts; only each event's position is a
        // `Sequence`, so the span is computed in integers and crossed per item.
        let from = after.as_i64().saturating_add(1);
        if from > self.through.as_i64() {
            return Ok(Vec::new());
        }
        let last = from
            .saturating_add(i64::try_from(max_events).unwrap_or(i64::MAX))
            .saturating_sub(1)
            .min(self.through.as_i64());
        Ok((from..=last).map(|s| event(s, self.tenant)).collect())
    }
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

async fn drain_events(session: &mut StreamSession, want: usize) -> Vec<Sequence> {
    let mut found = Vec::new();
    // Generous bound: each poll may return a topology or heartbeat frame, and
    // the loader needs a few ticks to fetch. Bounded so a stall fails.
    for _ in 0..(want * 8 + 64) {
        match tokio::time::timeout(Duration::from_millis(500), session.next_frame()).await {
            Ok(Some(Frame::Event(event))) => {
                if let Some(sequence) = event.sequence {
                    found.push(sequence);
                }
                if found.len() >= want {
                    return found;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return found,
        }
    }
    found
}

/// Nothing in this test absorbs. If events arrive, the loader fetched them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_loader_fills_a_cache_a_session_is_reading() {
    let tenant = Uuid::new_v4();
    let topics = Arc::new(TopicManager::new(TopicPolicy::default()));
    let source = Arc::new(Stored {
        through: Sequence::assigned(12),
        tenant,
        reads: AtomicI64::new(0),
    });
    let scheduler = Arc::new(DemandScheduler::new(
        Arc::clone(&source),
        Arc::clone(&topics),
        SchedulerPolicy::with_pool(4).build(),
    ));

    let ready = Arc::new(Notify::new());
    let leases = Arc::new(InProcessStreamLeases::new());
    let lease = leases.acquire(Uuid::new_v4()).expect("lease is free");
    let slots = attach_readers(&AttachRequest {
        topics: &topics,
        assigned: &[Assignment {
            topic: gts(TOPIC),
            partition: 0,
            offset: Sequence::assigned(0),
            last_examined: Sequence::assigned(0),
        }],
        cursors: &[],
        ready: &ready,
    });

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

    let delivered = drain_events(&mut session, 12).await;

    shutdown.cancel();
    loader.await.expect("loader task did not panic");

    assert_eq!(
        delivered,
        (1..=12).map(Sequence::assigned).collect::<Vec<Sequence>>(),
        "the loader must have fetched and absorbed every event, in order, with \
         nothing in this test calling absorb"
    );
    assert!(
        source.reads.load(Ordering::Relaxed) > 0,
        "no backend read was issued at all"
    );
}

/// The loader must reclaim, not only fetch. `run_round` returns early when no
/// partition wants anything, so reclamation cannot live inside it - a quiet
/// partition holding spans its reader has already passed is exactly the case
/// that needs freeing, and exactly the case a fetch round skips.
///
/// The reader is deliberately present and ahead of the data. With *no* readers
/// `reclaim` frees nothing but byte pressure, on purpose: a partition whose last
/// reader deregistered for an instant during a rebalance must not be flushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_loader_reclaims_spans_a_reader_has_passed() {
    let tenant = Uuid::new_v4();
    let topics = Arc::new(TopicManager::new(TopicPolicy::default()));
    let source = Arc::new(Stored {
        through: Sequence::assigned(0),
        tenant,
        reads: AtomicI64::new(0),
    });
    let scheduler = Arc::new(DemandScheduler::new(
        Arc::clone(&source),
        Arc::clone(&topics),
        SchedulerPolicy::with_pool(2).build(),
    ));

    let key = PartitionKey::new(gts(TOPIC), 0);
    let partition = topics.attach(&key);
    let segment = Segment::builder()
        .from(Sequence::assigned(1))
        .through(Sequence::assigned(64))
        .events((1..=64).map(|s| event(s, tenant)).collect())
        .build();
    partition
        .cache()
        .absorb(AbsorbedFetch::builder(segment).build());

    // A reader seeded past the whole segment, so every resident span is below
    // it and therefore dead by the policy's own definition.
    let ready = Arc::new(Notify::new());
    // Held for the duration: dropping the slots would deregister the reader,
    // and `reclaim` then frees nothing but byte pressure by design.
    let slots = attach_readers(&AttachRequest {
        topics: &topics,
        assigned: &[Assignment {
            topic: gts(TOPIC),
            partition: 0,
            offset: Sequence::assigned(0),
            last_examined: Sequence::assigned(0),
        }],
        cursors: &[crate::domain::model::Cursor {
            topic: gts(TOPIC),
            consumer_group: gts(
                "gts.cf.core.events.consumer_group.v1~02943530-10da-4624-a3ae-b998c425847f",
            ),
            partition: 0,
            offset: Sequence::assigned(64),
        }],
        ready: &ready,
    });

    let before = partition.cache().stats().resident().bytes();
    assert!(before > 0, "the fixture must actually be resident");

    let shutdown = CancellationToken::new();
    let loader = ShardLoader::new(
        Arc::clone(&scheduler),
        Arc::clone(&topics),
        Duration::from_millis(5),
    )
    .spawn(shutdown.clone());

    tokio::time::sleep(Duration::from_millis(80)).await;
    shutdown.cancel();
    loader.await.expect("loader task did not panic");

    assert_eq!(
        partition.cache().stats().resident().bytes(),
        0,
        "every span sits below the reader, so an idle loader pass must free all \
         {before} bytes; freeing nothing means reclamation is not being driven"
    );
    assert!(
        partition.cache().stats().balances(),
        "accounting must still balance after the loader reclaimed"
    );

    drop(slots);
}
