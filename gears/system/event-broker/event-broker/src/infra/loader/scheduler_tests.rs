//! The scheduler against a synthetic backend.
//!
//! The backend models two things a zero-latency stub would not. It has a
//! **connection cost**, so the pool can actually be the binding constraint and
//! its utilisation is observable. And it separates when an event is *appended*
//! from when it becomes *visible to a read*, because a cluster notification can
//! arrive before the backend has assigned the sequence - which is the condition
//! that makes the tail poller load-bearing rather than an optimisation.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::Utc;
use serde_json::json;
use tokio::time::Instant;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::model::{Event, Sequence};
use crate::domain::streaming::read::PartitionRead;
use crate::domain::streaming::read::{MaxBytes, MaxEvents, ReadLimit};
use crate::domain::streaming::source::PartitionKey;
use crate::infra::partition_cache::reclaim::{
    GapThresholdEvents, ReclaimPolicy, ResidencyLimitBytes,
};

use super::poll::PollPolicy;
use super::scheduler::{DemandScheduler, SchedulerPolicy};
use super::source::{EventSource, SourceError};
use super::topics::{TopicManager, TopicPolicy};

fn topic() -> GtsInstanceId {
    GtsInstanceId::try_new("gts.cf.core.events.topic.v1~x.eb.orders.acme.v1")
        .expect("static topic id is valid")
}

fn key(partition: i32) -> PartitionKey {
    PartitionKey::new(topic(), partition)
}

fn event(sequence: i64) -> Event {
    let sequence = Sequence::assigned(sequence);
    Event {
        id: Uuid::nil(),
        r#type: crate::test_support::event_type_id(
            "gts.cf.core.events.event.v1~x.eb.o.created.v1~",
        ),
        topic: topic(),
        tenant_id: Uuid::nil(),
        source: "scheduler".to_owned(),
        subject: "order".to_owned(),
        subject_type: "order".to_owned(),
        occurred_at: Utc::now(),
        trace_parent: None,
        data: json!({ "n": sequence }),
        meta: None,
        partition: Some(0),
        sequence: Some(sequence),
        sequence_time: None,
    }
}

/// Each event with the instant it becomes readable.
type VisibleLog = HashMap<PartitionKey, Vec<(Instant, Event)>>;

/// Long enough that nothing under test will ever see it.
const NEVER_VISIBLE: Duration = Duration::from_hours(1);

/// A backend with a cost and a visibility delay.
struct Synthetic {
    log: Mutex<VisibleLog>,
    /// How long after being appended an event can be read. Models a
    /// notification that outruns the sequence being assigned.
    visibility_gap: Duration,
    latency: Duration,
    reads: AtomicUsize,
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
}

impl Synthetic {
    fn new(visibility_gap: Duration) -> Self {
        Self {
            log: Mutex::new(HashMap::new()),
            visibility_gap,
            latency: Duration::from_millis(2),
            reads: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            peak_in_flight: AtomicUsize::new(0),
        }
    }

    /// Appends `from..=through`, visible only after the gap.
    fn append(&self, key: &PartitionKey, from: i64, through: i64) {
        let from = Sequence::assigned(from);
        let through = Sequence::assigned(through);
        let visible_at = Instant::now() + self.visibility_gap;
        let mut log = self.log.lock().expect("test lock");
        let partition = log.entry(key.clone()).or_default();
        for sequence in from.as_i64()..=through.as_i64() {
            partition.push((visible_at, event(sequence)));
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    fn peak_in_flight(&self) -> usize {
        self.peak_in_flight.load(Ordering::Relaxed)
    }
}

impl EventSource for Synthetic {
    fn read(
        &self,
        key: &PartitionKey,
        after: Sequence,
        max_events: usize,
    ) -> impl Future<Output = Result<Vec<Event>, SourceError>> + Send {
        let key = key.clone();
        async move {
            let depth = self
                .in_flight
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            self.peak_in_flight.fetch_max(depth, Ordering::Relaxed);
            self.reads.fetch_add(1, Ordering::Relaxed);

            tokio::time::sleep(self.latency).await;

            let now = Instant::now();
            // The guard is taken after the await and dropped before returning.
            let found = {
                let log = self.log.lock().expect("test lock");
                log.get(&key)
                    .map(|events| {
                        events
                            .iter()
                            .filter(|(visible_at, _)| now >= *visible_at)
                            .filter(|(_, event)| event.sequence.is_some_and(|at| at > after))
                            .take(max_events)
                            .map(|(_, event)| event.clone())
                            .collect::<Vec<Event>>()
                    })
                    .unwrap_or_default()
            };

            self.in_flight.fetch_sub(1, Ordering::Relaxed);
            Ok(found)
        }
    }
}

fn topic_policy() -> TopicPolicy {
    TopicPolicy::builder(ReclaimPolicy::new(
        GapThresholdEvents(8192),
        ResidencyLimitBytes(64 * 1024 * 1024),
    ))
    .fetch_max_events(256)
    .poll(PollPolicy::from_floor(Duration::from_millis(5)).up_to(Duration::from_millis(40)))
    .build()
}

fn scheduler(source: Arc<Synthetic>, pool: usize) -> DemandScheduler<Synthetic> {
    DemandScheduler::new(
        source,
        Arc::new(TopicManager::new(topic_policy())),
        SchedulerPolicy::with_pool(pool).build(),
    )
}

fn read_limit() -> ReadLimit {
    ReadLimit::new(MaxEvents(256), MaxBytes(1024 * 1024))
}

#[tokio::test]
async fn a_thousand_readers_on_one_partition_cost_one_fetch() {
    let source = Arc::new(Synthetic::new(Duration::ZERO));
    let loader = scheduler(Arc::clone(&source), 16);
    let partition = loader.topics().attach(&key(0));
    let _readers: Vec<_> = (0..1000)
        .map(|_| partition.cache().track_reader(Sequence::assigned(0)))
        .collect();
    source.append(&key(0), 1, 256);

    let report = loader.run_round().await;

    // The claim the whole architecture rests on.
    assert_eq!(report.fetches_issued(), 1);
    assert_eq!(report.readers_served(), 1000);
    assert_eq!(
        source.reads(),
        1,
        "one read against the backend, not a thousand"
    );
}

#[tokio::test]
async fn fetches_track_partitions_rather_than_readers() {
    let source = Arc::new(Synthetic::new(Duration::ZERO));
    let loader = scheduler(Arc::clone(&source), 16);
    let mut readers = Vec::new();
    for partition in 0..16 {
        let held = loader.topics().attach(&key(partition));
        for _ in 0..64 {
            readers.push(held.cache().track_reader(Sequence::assigned(0)));
        }
        source.append(&key(partition), 1, 256);
    }

    let report = loader.run_round().await;

    assert_eq!(report.partitions_scanned(), 16);
    assert_eq!(
        report.fetches_issued(),
        16,
        "one per partition, not per reader"
    );
    assert_eq!(report.readers_served(), 1024);
    // 1024 readers served by 16 fetches: the coalescing ratio, asserted by
    // multiplication because integer division is denied workspace-wide.
    assert!(report.readers_served() >= report.fetches_issued() * 64);
}

#[tokio::test]
async fn a_partition_with_a_fetch_outstanding_is_not_fetched_again() {
    let source = Arc::new(Synthetic::new(Duration::ZERO));
    let loader = scheduler(Arc::clone(&source), 16);
    let partition = loader.topics().attach(&key(0));
    let _reader = partition.cache().track_reader(Sequence::assigned(0));
    source.append(&key(0), 1, 256);

    assert!(partition.claim(), "stand in for a worker already fetching");
    let report = loader.run_round().await;

    assert_eq!(report.fetches_issued(), 0);
    assert_eq!(report.suppressed_in_flight(), 1);
    assert_eq!(source.reads(), 0);
}

#[tokio::test]
async fn the_pool_bounds_how_many_fetches_run_at_once() {
    let source = Arc::new(Synthetic::new(Duration::ZERO));
    let loader = scheduler(Arc::clone(&source), 4);
    // Held rather than leaked: dropping a handle deregisters the reader, and a
    // partition with no readers produces no demand.
    let mut readers = Vec::new();
    for partition in 0..32 {
        let held = loader.topics().attach(&key(partition));
        readers.push(held.cache().track_reader(Sequence::assigned(0)));
        source.append(&key(partition), 1, 16);
    }

    let report = loader.run_round().await;

    assert_eq!(
        report.fetches_issued(),
        32,
        "every partition wants something"
    );
    assert!(
        source.peak_in_flight() <= 4,
        "the pool is four connections, but {} reads overlapped",
        source.peak_in_flight()
    );
    drop(readers);
}

#[tokio::test]
async fn an_event_not_yet_visible_is_found_by_a_later_poll() {
    // The notification outruns the backend: the event is appended but cannot be
    // read for another 30ms.
    let source = Arc::new(Synthetic::new(Duration::from_millis(30)));
    let loader = scheduler(Arc::clone(&source), 16);
    let partition = loader.topics().attach(&key(0));
    let reader = partition.cache().track_reader(Sequence::assigned(0));
    source.append(&key(0), 1, 16);

    let first = loader.run_round().await;
    assert_eq!(first.fetches_issued(), 1);
    assert_eq!(
        first.empty_fetches(),
        1,
        "the sequence exists but is not yet readable - not an error, and not \
         proof the partition is idle"
    );
    assert!(matches!(
        partition
            .cache()
            .read_from(Sequence::assigned(0), read_limit()),
        PartitionRead::Unknown
    ));

    // Without a poller this reader would wait for a notification that already
    // fired, and wait forever.
    let mut served = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if loader.run_round().await.events_fetched() > 0 {
            served = true;
            break;
        }
    }

    assert!(
        served,
        "the poller must eventually find what the notification promised"
    );
    assert!(matches!(
        partition
            .cache()
            .read_from(Sequence::assigned(0), read_limit()),
        PartitionRead::Hit { .. }
    ));
    drop(reader);
}

#[tokio::test]
async fn an_empty_tail_backs_off_instead_of_asking_every_round() {
    // Nothing will ever become visible, so every tail fetch comes back empty.
    let source = Arc::new(Synthetic::new(NEVER_VISIBLE));
    let loader = scheduler(Arc::clone(&source), 16);
    let partition = loader.topics().attach(&key(0));
    let _reader = partition.cache().track_reader(Sequence::assigned(0));
    source.append(&key(0), 1, 16);

    let mut rounds = 0;
    let mut deferred = 0;
    for _ in 0..12 {
        let report = loader.run_round().await;
        rounds += 1;
        deferred += report.deferred_by_backoff();
    }

    assert!(
        deferred > 0,
        "after {rounds} rounds against a tail that never appears, some rounds \
         must have been skipped rather than hammering the backend"
    );
    assert!(
        source.reads() < rounds,
        "reads {} should be fewer than rounds {rounds}",
        source.reads()
    );
}
