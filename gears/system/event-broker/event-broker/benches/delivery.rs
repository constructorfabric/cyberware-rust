//! Profile-driven fan-out benchmark for the streaming pipeline.
//!
//! This is the whole mechanism running against itself: consumer groups reading
//! partitions of several topics, a loader turning what those readers want into
//! fetches, one small connection pool shared by every topic, and the background
//! jobs that keep memory bounded while it happens.
//!
//! **The claim under measurement.** A thousand consumer groups reading the same
//! partition are a thousand *registrations* on one cache, not a thousand
//! fetches. Demands are derived per partition rather than reported per reader,
//! so the number of fetches tracks the partition count and the number of reader
//! *clusters* within each - never the reader count. Against a pool of sixteen
//! connections shared across every topic, that ratio is the difference between
//! working and not: uncoalesced, sixty thousand readers refilling a few times a
//! second want two orders of magnitude more connections than exist.
//!
//! **What each iteration does.** Streams every partition's events end to end
//! while every reader consumes all of them, then verifies the run before it is
//! allowed to count. Verification, not timing, is what makes this a benchmark
//! rather than a demonstration: each reader checks every sequence it is handed
//! against the one it is owed, so an event delivered late, twice, or not at all
//! fails the iteration.
//!
//! **Profiles.** Each benchmark is a [`FanOutProfile`] - a declarative
//! description of the topology, the reader pattern, the pool, and the background
//! jobs. One runner ([`run_profile`]) executes any of them.
//!
//! **Tiers**, selected by environment variable:
//!   - Validation (default) - small topologies, seconds per profile.
//!   - Longhaul (`BENCH_LONGHAUL=1`) - the capacity design point: a thousand
//!     groups, sixteen partitions a topic, a sixteen-connection pool.
//!   - Stress (`BENCH_STRESS=1`) - dispersed readers, laggards, a residency
//!     limit tight enough to force constant reclamation, and a backend whose
//!     events are not readable the instant they are appended.
//!
//! **Run examples:**
//!   ```sh
//!   cargo bench -p cf-gears-event-broker --bench delivery
//!   cargo bench -p cf-gears-event-broker --bench delivery -- tail_64g
//!   BENCH_LONGHAUL=1 cargo bench -p cf-gears-event-broker --bench delivery
//!   BENCH_STRESS=1 cargo bench -p cf-gears-event-broker --bench delivery
//!   ```

#![allow(clippy::expect_used, clippy::missing_panics_doc)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use chrono::Utc;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use gts::GtsTypeId;
use serde_json::json;
use tokio::runtime::Runtime;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio::time::Instant;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use event_broker::domain::consumer_group_coordinator::{ConsumerGroupCoordinator, TopicInterest};
use event_broker::domain::model::{
    Assignment, BarrierMode, Cursor, Event, Interest, Sequence, TenantTraversalDepth,
};
use event_broker::domain::streaming::filter::{EventFilter, InterestFilter};
use event_broker::domain::streaming::frames::Frame;
use event_broker::domain::streaming::lease::{InProcessStreamLeases, StreamLeases};
use event_broker::domain::streaming::progress::ProgressConfig;
use event_broker::domain::streaming::read::{MaxBytes, MaxEvents, ReadLimit};
use event_broker::domain::streaming::read_set::ReadSet;
use event_broker::domain::streaming::session::{SessionOpening, StreamSession};
use event_broker::domain::streaming::source::PartitionKey;
use event_broker::domain::streaming::time::NowFn;
use event_broker::infra::loader::attach::{AttachRequest, attach_readers};
use event_broker::infra::loader::poll::PollPolicy;
use event_broker::infra::loader::scheduler::{DemandScheduler, SchedulerPolicy};
use event_broker::infra::loader::source::{EventSource, SourceError};
use event_broker::infra::loader::topics::{TopicManager, TopicPolicy};
use event_broker::infra::partition_cache::demand::StarvationWeight;
use event_broker::infra::partition_cache::reclaim::{
    GapThresholdEvents, ReclaimPolicy, ResidencyLimitBytes,
};

// ---------------------------------------------------------------------------
// Profile - the single source of truth for one benchmark
// ---------------------------------------------------------------------------

/// When a profile runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    Validation,
    Longhaul,
    Stress,
}

/// Where a group's readers start, which is what decides how many clusters a
/// partition has and therefore how many fetches it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderPattern {
    /// Every group tracks the producer. One cluster a partition, so one fetch
    /// serves every group - the case the design is built for.
    Tail,
    /// Every group replays from the beginning while the producer runs ahead.
    /// Reclamation has to keep up with readers rather than with appends.
    Sweep,
    /// Groups scattered across the stream, so a partition has several clusters
    /// and the loader has to choose between them.
    Dispersed,
    /// Most groups at the tail, one in sixteen far behind. Bounded residency is
    /// not available here; the laggards pay in refetches instead.
    TailWithLaggards,
}

/// What the pool is expected to be during a run.
///
/// Declared rather than observed, because "the pool was saturated" and "the pool
/// had room" are the two halves of the same claim and a profile that cannot say
/// which it is measures neither. A run that saturates when it was supposed to
/// have headroom is bottlenecked somewhere the profile did not intend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolExpectation {
    /// Every connection busy: the pool is the binding constraint.
    Saturated,
    /// Connections to spare, so throughput reflects the rest of the pipeline.
    Headroom,
}

/// Which background jobs run.
#[derive(Debug, Clone, Copy)]
struct BackgroundJobs {
    /// Reclamation passes on a tick, alongside the inline enforcement every
    /// absorb already does.
    reclaim: bool,
    /// How often those passes run.
    reclaim_interval: Duration,
}

#[derive(Debug, Clone, Copy)]
struct CriterionSettings {
    samples: usize,
    measurement: Duration,
    warmup: Duration,
}

#[derive(Debug, Clone, Copy)]
struct FanOutProfile {
    /// Shown in criterion output.
    name: &'static str,
    tier: Tier,

    // -- Topology --
    /// Distinct topics in the instance.
    topics: usize,
    partitions_per_topic: usize,
    /// Consumer groups. A group's members hold disjoint partitions, so one group
    /// reads each of its partitions exactly once - which makes this the number
    /// of readers on every partition it subscribes to.
    groups: usize,
    /// How many of the topics each group subscribes to.
    topics_per_group: usize,

    // -- Workload --
    /// A count, not a position: how many events each partition carries.
    events_per_partition: i64,
    reader_pattern: ReaderPattern,

    // -- Loader --
    /// Connections shared across every topic and partition. Small on purpose:
    /// it is the constraint coalescing exists to respect.
    pool_permits: usize,
    pool_expectation: PoolExpectation,
    fetch_max_events: usize,
    starvation_weight: StarvationWeight,

    // -- Backend --
    backend_latency: Duration,
    /// How long after an append an event becomes readable. Non-zero models a
    /// cluster notification outrunning the sequence being assigned, which is
    /// what makes the tail poller load-bearing.
    visibility_gap: Duration,

    // -- Memory --
    /// Events a partition may hold, converted to bytes once one event has been
    /// measured.
    residency_events: usize,
    gap_threshold_events: usize,

    background: BackgroundJobs,

    // -- Expectations, asserted every iteration --
    /// Deliveries every backend read must be worth. This is the coalescing
    /// ratio stated as a floor: one read brings at most `fetch_max_events` into
    /// one partition, and every reader on that partition is served from it, so
    /// a read that pays for itself yields many multiples of itself.
    min_deliveries_per_read: u64,
    /// Scans any one reader may go unserved. Bounds fairness: an unbounded
    /// maximum is the failure that a throughput number hides.
    max_starvation: u32,

    criterion: CriterionSettings,
}

impl FanOutProfile {
    /// Partitions in the instance.
    fn partitions(self) -> usize {
        self.topics * self.partitions_per_topic
    }

    /// Partitions one group holds.
    fn partitions_per_group(self) -> usize {
        self.topics_per_group * self.partitions_per_topic
    }

    /// A generous ceiling on one iteration, so a stalled run fails rather than
    /// hanging the suite.
    fn deadline(self) -> Duration {
        Duration::from_secs(120)
    }

    /// Whether the stream is larger than a partition may hold.
    ///
    /// Derived rather than declared, because getting it wrong in either
    /// direction hides a real result. Asserting reclamation when the whole
    /// stream fits in the cache fails a run that was correct; *not* asserting it
    /// when the stream is four times the residency lets a run that quietly blew
    /// its memory bound pass as a success.
    fn must_reclaim(self) -> bool {
        self.background.reclaim
            && self.events_per_partition > i64::try_from(self.residency_events).unwrap_or(i64::MAX)
    }
}

// ---------------------------------------------------------------------------
// The backend: one pool, a cost per read, and a visibility delay
// ---------------------------------------------------------------------------

/// Each event paired with the instant it becomes readable.
type VisibleLog = HashMap<PartitionKey, Vec<(Instant, Event)>>;

/// Stands in for storage.
///
/// Two things a zero-cost stub would hide. Reads take time, so the pool can
/// actually bind and its occupancy is observable. And an appended event is not
/// immediately readable, because a notification can arrive before the backend
/// has assigned the sequence - so a fetch aimed at the tail can legitimately
/// come back empty, and the loader has to keep asking rather than concluding the
/// partition is idle.
struct SharedBackend {
    log: Mutex<VisibleLog>,
    visibility_gap: Duration,
    latency: Duration,
    reads: AtomicUsize,
    empty_reads: AtomicUsize,
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
}

impl SharedBackend {
    fn new(profile: FanOutProfile) -> Self {
        Self {
            log: Mutex::new(HashMap::new()),
            visibility_gap: profile.visibility_gap,
            latency: profile.backend_latency,
            reads: AtomicUsize::new(0),
            empty_reads: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            peak_in_flight: AtomicUsize::new(0),
        }
    }

    fn append(&self, key: &PartitionKey, from: i64, through: i64) {
        let visible_at = Instant::now() + self.visibility_gap;
        let mut log = self.log.lock().expect("backend log");
        let partition = log.entry(key.clone()).or_default();
        for sequence in from..=through {
            partition.push((visible_at, event(key, sequence)));
        }
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    fn empty_reads(&self) -> usize {
        self.empty_reads.load(Ordering::Relaxed)
    }

    fn peak_in_flight(&self) -> usize {
        self.peak_in_flight.load(Ordering::Relaxed)
    }
}

impl EventSource for SharedBackend {
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
            // Taken after the await and dropped before returning, so the lock is
            // never held across a suspension point.
            let found = {
                let log = self.log.lock().expect("backend log");
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

            if found.is_empty() {
                self.empty_reads.fetch_add(1, Ordering::Relaxed);
            }
            self.in_flight.fetch_sub(1, Ordering::Relaxed);
            Ok(found)
        }
    }
}

/// The event type every fixture event carries, and the only one any session's
/// interests name - so the filter matches everything and the delivered count is
/// the fan-out work, not a filtering artefact.
const EVENT_TYPE: &str = "gts.cf.core.events.event.v1~x.eb.o.created.v1~";

fn topic_id(index: usize) -> GtsInstanceId {
    GtsInstanceId::try_new(&format!(
        "gts.cf.core.events.topic.v1~x.eb.t{index}.acme.v1"
    ))
    .expect("generated topic id is valid")
}

fn event(key: &PartitionKey, sequence: i64) -> Event {
    let sequence = Sequence::assigned(sequence);
    Event {
        id: Uuid::nil(),
        r#type: GtsTypeId::try_new(EVENT_TYPE).expect("static gts type id is valid"),
        topic: key.topic.clone(),
        tenant_id: Uuid::nil(),
        source: "delivery".to_owned(),
        subject: "order".to_owned(),
        subject_type: "order".to_owned(),
        occurred_at: Utc::now(),
        trace_parent: None,
        data: json!({ "n": sequence, "body": "a representative payload" }),
        meta: None,
        partition: Some(key.partition),
        sequence: Some(sequence),
        sequence_time: None,
    }
}

// ---------------------------------------------------------------------------
// The consumer - the real session
// ---------------------------------------------------------------------------

/// One consumer group's session, driven through the production
/// `StreamSession`.
///
/// This used to be a hand-rolled reader loop standing in for the session, which
/// meant the benchmark measured a shape the service does not run and stopped
/// predicting anything the moment the two drifted. Driving the real session
/// widens what a run covers - `attach_readers`, `ReadSet` rotation, the filter,
/// frame construction and the park-and-wake path all sit inside the measurement
/// now - and removes the duplicate.
///
/// A group's members hold disjoint partitions, so a session is one reader per
/// partition it was assigned, spanning however many topics the group subscribed
/// to, multiplexed by a single task. That shape is why fan-out works: a thousand
/// groups reading one partition arrive as a thousand registrations on one cache,
/// and the loader collapses them into one fetch.
struct Session {
    session: StreamSession,
    /// The next sequence this session must be handed, per partition.
    ///
    /// What the session has actually *seen*, deliberately separate from the
    /// offsets the cache told its readers to advance to. Only this side can
    /// catch an event that was skipped or repeated, and the two agreeing is the
    /// property worth checking.
    expected: HashMap<(GtsInstanceId, i32), Sequence>,
    /// Events still owed before this session is finished.
    remaining: u64,
}

/// Opens one session over `keys`, seeded from a cursor at `start`.
fn open_session(
    topics: &TopicManager,
    leases: &Arc<InProcessStreamLeases>,
    groups: &Arc<ConsumerGroupCoordinator>,
    keys: &[PartitionKey],
    start: i64,
    profile: FanOutProfile,
) -> Session {
    let start = Sequence::assigned(start);
    let ready = Arc::new(Notify::new());
    let subscription_id = Uuid::new_v4();
    let lease = leases
        .acquire(subscription_id)
        .expect("a fresh subscription id is never leased");

    // One coordinator for the run, and **one group per session**, not one
    // shared group. A shared group makes the coordinator rebalance across every
    // session: each join reassigns partitions and publishes the change, so every
    // existing session immediately classifies a loss or a gain and either
    // narrows its read set or terminates - and the run never delivers what it
    // was owed. That is correct coordinator behaviour and the wrong model here,
    // because these profiles represent independent groups each holding their own
    // partitions.
    //
    // With one member per group, `join` assigns it every partition of its
    // topics, which is exactly what `group_partitions` produced - so the
    // published generation matches the attached slots and nothing rebalances.
    let group = GtsInstanceId::try_new(&format!(
        "gts.cf.core.events.consumer_group.v1~{}",
        Uuid::new_v4()
    ))
    .expect("a uuid instance part is a valid consumer group id");
    groups.join(
        &group,
        subscription_id,
        &keys
            .iter()
            .map(|key| TopicInterest {
                id: key.topic.clone(),
                partitions: i32::try_from(profile.partitions_per_topic).unwrap_or(i32::MAX),
            })
            .collect::<Vec<_>>(),
        Duration::from_secs(300),
    );
    let (generations, membership) =
        ConsumerGroupCoordinator::subscribe(groups, &group, subscription_id)
            .expect("the member just joined");

    let assigned: Vec<Assignment> = keys
        .iter()
        .map(|key| Assignment {
            topic: key.topic.clone(),
            partition: key.partition,
            offset: Sequence::assigned(0),
            last_examined: Sequence::assigned(0),
        })
        .collect();
    let cursors: Vec<Cursor> = keys
        .iter()
        .map(|key| Cursor {
            topic: key.topic.clone(),
            consumer_group: group.clone(),
            partition: key.partition,
            offset: start,
        })
        .collect();

    let slots = attach_readers(&AttachRequest {
        topics,
        assigned: &assigned,
        cursors: &cursors,
        ready: &ready,
    });

    // One interest per distinct topic the group holds, matching every event the
    // fixture produces. The filter is in the measurement deliberately: a real
    // session evaluates one per event, and leaving it out would understate the
    // per-event cost by exactly the amount a subscription actually pays.
    let mut topics_seen: Vec<GtsInstanceId> = Vec::new();
    for key in keys {
        if !topics_seen.contains(&key.topic) {
            topics_seen.push(key.topic.clone());
        }
    }
    let interests: Vec<Interest> = topics_seen
        .into_iter()
        .map(|topic| Interest {
            topic,
            tenant_id: Uuid::nil(),
            depth: TenantTraversalDepth::CurrentTenant,
            barrier_mode: BarrierMode::Respect,
            types: vec![EVENT_TYPE.to_owned()],
            filter: None,
        })
        .collect();
    let filter: Arc<dyn EventFilter> =
        Arc::new(InterestFilter::compile(&interests).expect("generated interests compile"));

    let remaining = u64::try_from(profile.events_per_partition.saturating_sub(start.as_i64()))
        .unwrap_or(0)
        .saturating_mul(u64::try_from(keys.len()).unwrap_or(0));

    let now: NowFn = Arc::new(Utc::now);
    let session = StreamSession::open(SessionOpening {
        read_set: ReadSet::seed(slots),
        filter,
        // Long cadences on purpose: a heartbeat or progress frame is not what
        // this measures, and a short one would put frame construction the
        // service does not do at this rate inside the numbers.
        progress: ProgressConfig::default(),
        heartbeat_interval: Duration::from_secs(300),
        limit: ReadLimit::new(MaxEvents(profile.fetch_max_events), MaxBytes(1024 * 1024)),
        topology_version: 1,
        ready,
        started_at: Instant::now(),
        now,
        unanswerable_tolerance: Duration::from_secs(300),
        lease,
        generations,
        membership,
    });

    Session {
        session,
        expected: keys
            .iter()
            .map(|key| {
                (
                    (key.topic.clone(), key.partition),
                    Sequence::assigned(start.as_i64().saturating_add(1)),
                )
            })
            .collect(),
        remaining,
    }
}

/// Drives one session until it has been handed everything it is owed.
///
/// Every event is checked against what this session was owed next on that
/// partition, so a skipped, repeated or out-of-order delivery fails the run
/// rather than quietly improving the throughput number.
async fn consume(mut consumer: Session) -> u64 {
    let mut delivered: u64 = 0;

    while consumer.remaining > 0 {
        let Some(frame) = consumer.session.next_frame().await else {
            break;
        };

        // Topology, heartbeat and progress frames are real output but carry no
        // events; they are counted by neither side.
        let Frame::Event(event) = frame else {
            continue;
        };

        let sequence = event
            .sequence
            .expect("a delivered event carries its sequence");
        let partition = event
            .partition
            .expect("a delivered event carries its partition");
        let key = (event.topic.clone(), partition);
        let owed = consumer
            .expected
            .get_mut(&key)
            .expect("an event arrived for a partition this session never held");
        assert_eq!(
            sequence, *owed,
            "session was handed {sequence} while owed {owed} on {key:?} - an \
             event was skipped, repeated, or delivered out of order"
        );
        *owed = Sequence::assigned(owed.as_i64().saturating_add(1));

        delivered = delivered.saturating_add(1);
        consumer.remaining = consumer.remaining.saturating_sub(1);
    }

    delivered
}

// ---------------------------------------------------------------------------
// Background jobs
// ---------------------------------------------------------------------------

/// What the loader did over a whole run.
#[derive(Default)]
struct LoaderTotals {
    /// High-water starvation, sampled per round.
    ///
    /// Sampled during the run, not read afterwards: a session's readers
    /// deregister when it drops, so by the time every session has finished the
    /// registry is empty and the counter is gone. Reading it at the end reported
    /// zero for every profile.
    worst_starvation: AtomicU32,
    rounds: AtomicUsize,
    fetches: AtomicUsize,
    readers_served: AtomicUsize,
    empty: AtomicUsize,
    deferred: AtomicUsize,
    suppressed: AtomicUsize,
}

/// Appends to the backend until every partition has its full stream.
async fn produce(backend: Arc<SharedBackend>, keys: Vec<PartitionKey>, profile: FanOutProfile) {
    let batch = i64::try_from(profile.fetch_max_events).unwrap_or(256);
    let mut produced: i64 = 0;
    while produced < profile.events_per_partition {
        let from = produced.saturating_add(1);
        let through = from
            .saturating_add(batch - 1)
            .min(profile.events_per_partition);
        for key in &keys {
            backend.append(key, from, through);
        }
        produced = through;
        tokio::task::yield_now().await;
    }
}

/// Runs demand scans until told to stop.
async fn drive_loader(
    loader: Arc<DemandScheduler<SharedBackend>>,
    topics: Arc<TopicManager>,
    totals: Arc<LoaderTotals>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        let report = loader.run_round().await;
        totals.rounds.fetch_add(1, Ordering::Relaxed);
        totals
            .fetches
            .fetch_add(report.fetches_issued(), Ordering::Relaxed);
        totals
            .readers_served
            .fetch_add(report.readers_served(), Ordering::Relaxed);
        totals
            .empty
            .fetch_add(report.empty_fetches(), Ordering::Relaxed);
        totals
            .deferred
            .fetch_add(report.deferred_by_backoff(), Ordering::Relaxed);
        totals
            .suppressed
            .fetch_add(report.suppressed_in_flight(), Ordering::Relaxed);
        let worst = topics
            .live()
            .iter()
            .map(|partition| partition.cache().worst_starvation())
            .max()
            .unwrap_or(0);
        totals.worst_starvation.fetch_max(worst, Ordering::Relaxed);

        // Paced, matching `ShardLoader`. A round that issued no fetch waits
        // before trying again, because spinning here does not make the loader
        // faster - it makes it a competitor for the runtime threads the producer
        // and the sessions need. Unpaced, one profile per run would stall for
        // exactly the session's 30s progress deadline while the loader burned
        // 1.28M rounds issuing 7400 fetches that came back empty, because the
        // producer task it was starving had not appended anything yet.
        if report.fetches_issued() == 0 {
            tokio::time::sleep(Duration::from_micros(200)).await;
        } else {
            tokio::task::yield_now().await;
        }
    }
}

/// Reclamation passes on a tick.
///
/// Throttled rather than spinning: an unthrottled pass holds each partition's
/// write lock back to back and starves the readers behind it, which would be a
/// property of this harness rather than of the cache.
async fn reclaim_periodically(
    topics: Arc<TopicManager>,
    interval: Duration,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        for partition in topics.live() {
            let _ = partition.cache().reclaim();
        }
        tokio::time::sleep(interval).await;
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// One event's measured footprint, so a residency limit can be stated in events
/// and converted once rather than written as a magic byte count that drifts
/// whenever the event shape changes.
fn footprint_of_one_event() -> usize {
    use event_broker::infra::partition_cache::segment::Segment;
    let key = PartitionKey::new(topic_id(0), 0);
    Segment::builder()
        .from(Sequence::assigned(1))
        .through(Sequence::assigned(1))
        .events(vec![event(&key, 1)])
        .build()
        .bytes()
}

/// Where each group starts reading, which is what the reader pattern *is*.
fn group_starts(profile: FanOutProfile) -> Vec<Sequence> {
    // The pattern is arithmetic over counts; only the result is a position.
    let events = profile.events_per_partition;
    (0..profile.groups)
        .map(|group| match profile.reader_pattern {
            ReaderPattern::Tail | ReaderPattern::Sweep => 0_i64,
            // Deterministic rather than random: a failure has to be replayable.
            ReaderPattern::Dispersed => {
                let step = i64::try_from(group).unwrap_or(0).saturating_mul(7919);
                step % events.max(1)
            }
            // One group in sixteen replays from the beginning while the rest sit
            // near the tail. Shifted rather than divided, since integer division
            // is denied workspace-wide.
            ReaderPattern::TailWithLaggards => {
                if group % 16 == 0 {
                    0
                } else {
                    events.saturating_sub(events >> 3)
                }
            }
        })
        .map(Sequence::assigned)
        .collect()
}

/// The partitions one group subscribes to.
///
/// Topics are handed out round-robin so groups spread across them rather than
/// every group piling onto topic zero.
fn group_partitions(profile: FanOutProfile, group: usize) -> Vec<PartitionKey> {
    (0..profile.topics_per_group)
        .flat_map(|offset| {
            let topic = topic_id((group + offset) % profile.topics);
            (0..profile.partitions_per_topic).map(move |partition| {
                PartitionKey::new(topic.clone(), i32::try_from(partition).unwrap_or(0))
            })
        })
        .collect()
}

/// How far above its residency limit a partition may legitimately sit.
///
/// One unremovable segment per distinct reader position plus one absorbed fetch,
/// over profiles that keep their readers within a few segments of each other.
const MAX_RESIDENCY_AMPLIFICATION: usize = 4;

/// How many times the resident set must turn over before a run is accepted as
/// having exercised reclamation.
const MIN_RESIDENCY_TURNOVERS: u64 = 10;

/// What one run produced, and what it cost.
struct Outcome {
    delivered: u64,
    expected: u64,
    reads: usize,
    empty_reads: usize,
    peak_in_flight: usize,
    rounds: usize,
    fetches: usize,
    readers_served: usize,
    worst_starvation: u32,
    reclaimed_events: u64,
    /// Fetches the tail poller deliberately held back. Distinguishes "the
    /// poller is pacing an unmaterialised tail" from "nothing wanted a fetch".
    deferred: usize,
    /// Demands dropped because a fetch for that partition was already in
    /// flight - the coalescing that makes fan-out cheap.
    suppressed: usize,
    /// The high-water mark of what any single partition held.
    peak_resident_bytes: u64,
    /// Wall time for the whole run, so the table can carry a rate rather than
    /// leaving the counts to be cross-referenced against criterion's output.
    ///
    /// `std::time::Instant`, not tokio's: the profiles that model a visibility
    /// gap advance tokio's clock, so a tokio instant would measure simulated
    /// time and report a rate the machine never achieved.
    elapsed: Duration,
}

async fn run_once(profile: FanOutProfile) -> Outcome {
    // Wall time, deliberately: the late-visibility profiles advance tokio's
    // clock, so a tokio instant would time simulated seconds.
    let started = std::time::Instant::now();
    let backend = Arc::new(SharedBackend::new(profile));
    let residency_bytes = profile
        .residency_events
        .saturating_mul(footprint_of_one_event());
    let policy = TopicPolicy::builder(ReclaimPolicy::new(
        GapThresholdEvents(profile.gap_threshold_events),
        ResidencyLimitBytes(residency_bytes),
    ))
    .fetch_max_events(profile.fetch_max_events)
    .poll(PollPolicy::default())
    .build();

    let topics = Arc::new(TopicManager::new(policy));
    let loader = Arc::new(DemandScheduler::new(
        Arc::clone(&backend),
        Arc::clone(&topics),
        SchedulerPolicy::with_pool(profile.pool_permits)
            .starvation_weight(profile.starvation_weight)
            .build(),
    ));

    let all_keys: Vec<PartitionKey> = (0..profile.topics)
        .flat_map(|topic| {
            let id = topic_id(topic);
            (0..profile.partitions_per_topic).map(move |partition| {
                PartitionKey::new(id.clone(), i32::try_from(partition).unwrap_or(0))
            })
        })
        .collect();

    // A sweep replays a stream that already exists, so everything is appended
    // before anyone reads. Reclamation then has to keep up with readers rather
    // than with appends, which is the harder of the two.
    if profile.reader_pattern == ReaderPattern::Sweep {
        for key in &all_keys {
            backend.append(key, 1, profile.events_per_partition);
        }
    }

    let starts = group_starts(profile);
    let leases = Arc::new(InProcessStreamLeases::new());
    let groups = Arc::new(ConsumerGroupCoordinator::new());
    let mut expected: u64 = 0;
    let mut sessions = Vec::with_capacity(profile.groups);
    for (group, start) in starts.iter().copied().enumerate() {
        let keys = group_partitions(profile, group);
        let opened = open_session(&topics, &leases, &groups, &keys, start.as_i64(), profile);
        expected = expected.saturating_add(opened.remaining);
        sessions.push(opened);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let totals = Arc::new(LoaderTotals::default());

    let producer = (profile.reader_pattern != ReaderPattern::Sweep)
        .then(|| tokio::spawn(produce(Arc::clone(&backend), all_keys.clone(), profile)));
    let loader_job = tokio::spawn(drive_loader(
        Arc::clone(&loader),
        Arc::clone(&topics),
        Arc::clone(&totals),
        Arc::clone(&stop),
    ));
    let reclaimer = profile.background.reclaim.then(|| {
        tokio::spawn(reclaim_periodically(
            Arc::clone(&topics),
            profile.background.reclaim_interval,
            Arc::clone(&stop),
        ))
    });

    let mut running: JoinSet<u64> = JoinSet::new();
    for session in sessions {
        running.spawn(consume(session));
    }

    let mut delivered: u64 = 0;
    while let Some(joined) = running.join_next().await {
        delivered = delivered.saturating_add(joined.expect("a session task panicked"));
    }

    let worst_starvation = totals.worst_starvation.load(Ordering::Relaxed);

    // Only once every session has finished: the background jobs are what feed
    // them, so stopping earlier would deadlock the run.
    stop.store(true, Ordering::Relaxed);
    if let Some(producer) = producer {
        producer.await.expect("producer panicked");
    }
    loader_job.await.expect("loader panicked");
    if let Some(reclaimer) = reclaimer {
        reclaimer.await.expect("reclaimer panicked");
    }

    let reclaimed_events = topics
        .live()
        .iter()
        .map(|partition| partition.cache().stats().reclaimed().events())
        .fold(0, u64::saturating_add);
    // Per partition rather than summed: the residency limit is a per-partition
    // bound, and a shard-wide total would let one partition blow it while the
    // average stayed respectable.
    let peak_resident_bytes = topics
        .live()
        .iter()
        .map(|partition| partition.cache().stats().peak().bytes())
        .max()
        .unwrap_or(0);

    // Every partition's accounting must still balance after all of that.
    for partition in topics.live() {
        assert!(
            partition.cache().stats().balances(),
            "accounting for {:?} does not balance",
            partition.key()
        );
    }

    Outcome {
        delivered,
        expected,
        reads: backend.reads(),
        empty_reads: backend.empty_reads(),
        peak_in_flight: backend.peak_in_flight(),
        rounds: totals.rounds.load(Ordering::Relaxed),
        fetches: totals.fetches.load(Ordering::Relaxed),
        readers_served: totals.readers_served.load(Ordering::Relaxed),
        worst_starvation,
        reclaimed_events,
        peak_resident_bytes,
        deferred: totals.deferred.load(Ordering::Relaxed),
        suppressed: totals.suppressed.load(Ordering::Relaxed),
        elapsed: started.elapsed(),
    }
}

/// Everything an iteration has to satisfy before it is allowed to count.
fn verify(outcome: &Outcome, profile: FanOutProfile) {
    assert_eq!(
        outcome.delivered, outcome.expected,
        "every reader must receive every event of every partition it holds, \
         exactly once and in order"
    );
    assert!(
        outcome.peak_in_flight <= profile.pool_permits,
        "the pool is {} connections shared across every topic, but {} reads \
         overlapped",
        profile.pool_permits,
        outcome.peak_in_flight
    );
    match profile.pool_expectation {
        PoolExpectation::Saturated => assert_eq!(
            outcome.peak_in_flight, profile.pool_permits,
            "this profile exists to be pool-bound, and the pool never filled"
        ),
        PoolExpectation::Headroom => assert!(
            outcome.peak_in_flight < profile.pool_permits,
            "this profile exists to run with connections to spare, but all {} \
             were busy - whatever it measured, it was not the pipeline",
            profile.pool_permits
        ),
    }
    assert!(
        outcome.delivered
            >= u64::try_from(outcome.reads)
                .unwrap_or(u64::MAX)
                .saturating_mul(profile.min_deliveries_per_read),
        "coalescing floor missed: {} deliveries from {} backend reads, needed \
         {} per read",
        outcome.delivered,
        outcome.reads,
        profile.min_deliveries_per_read
    );
    assert!(
        outcome.worst_starvation <= profile.max_starvation,
        "a reader went unserved for {} scans, bound is {}",
        outcome.worst_starvation,
        profile.max_starvation
    );
    // The residency limit is a target the reclaim policy will deliberately miss,
    // so the bound asserted here is a multiple of it rather than the limit plus
    // one fetch. Two mechanisms put residency above the limit, and neither is a
    // leak:
    //
    //   - a batch is absorbed before it can be trimmed, so the high-water mark
    //     always leads the limit by up to one fetch, and
    //   - pressure never takes a segment some reader reads next, so there is one
    //     unremovable segment per *distinct* reader position. Readers on one
    //     partition consume at slightly different rates and so drift apart the
    //     longer a run goes, which makes the floor a function of reader spread
    //     rather than of the limit. Measured on `memory_bound_64g` against a
    //     256-event limit: 384 events resident at 1024 events a partition, 512
    //     at 2048, 704 at 4096.
    //
    // A hard cap is therefore not something this design offers a partition with
    // many readers at many positions, and asserting one would be asserting a
    // falsehood. What is worth pinning is that the amplification stays a small
    // constant - these profiles hold their readers within a few segments of each
    // other - so unbounded growth still fails the run.
    let allowed = u64::try_from(
        profile
            .residency_events
            .saturating_mul(footprint_of_one_event())
            .saturating_mul(MAX_RESIDENCY_AMPLIFICATION),
    )
    .unwrap_or(u64::MAX);
    assert!(
        outcome.peak_resident_bytes <= allowed,
        "a partition held {} bytes, more than {}x its {}-event residency limit - \
         at that point residency tracks something other than reader spread",
        outcome.peak_resident_bytes,
        MAX_RESIDENCY_AMPLIFICATION,
        profile.residency_events
    );
    if profile.must_reclaim() {
        // The load-bearing proof, and the one that fails outright with
        // reclamation switched off: the resident set turned over many times over
        // the run rather than the cache simply being large enough to hold
        // everything. The peak bound above is vacuously true for a run that
        // never filled the cache, which is why it cannot stand alone.
        let peak_events = outcome
            .peak_resident_bytes
            .checked_div(u64::try_from(footprint_of_one_event()).unwrap_or(1))
            .unwrap_or(0);
        assert!(
            outcome.reclaimed_events >= peak_events.saturating_mul(MIN_RESIDENCY_TURNOVERS),
            "{} events reclaimed against a peak residency of {} events - the \
             resident set has to turn over at least {} times for a {}-event \
             stream against a {}-event limit to have exercised reclamation at all",
            outcome.reclaimed_events,
            peak_events,
            MIN_RESIDENCY_TURNOVERS,
            profile.events_per_partition,
            profile.residency_events
        );
    }
}

/// Runs one profile once, outside any measurement.
///
/// This is where a profile that cannot satisfy its own expectations fails -
/// before criterion spends minutes measuring it - and where the mechanism
/// numbers come from, which timing alone would not show.
fn probe(runtime: &Runtime, profile: FanOutProfile) -> Outcome {
    runtime.block_on(async {
        tokio::time::timeout(profile.deadline(), run_once(profile))
            .await
            .expect("run did not finish within its deadline")
    })
}

/// Deliveries each backend read was worth.
fn per_read(outcome: &Outcome) -> u64 {
    // `checked_div` rather than `/`: integer division is denied workspace-wide,
    // and a profile that somehow issued no reads should report zero rather than
    // panic in the middle of a summary.
    outcome
        .delivered
        .checked_div(u64::try_from(outcome.reads).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Readers one fetch was expected to serve, as the loader accounted for it.
///
/// Complementary to `per read`, which the *backend* measures: this is the
/// coalescing the scheduler believed it achieved, that is what the readers
/// actually got out of it. The two disagreeing would mean a fetch counted
/// readers it did not go on to serve.
fn served_per_fetch(outcome: &Outcome) -> u64 {
    u64::try_from(outcome.readers_served)
        .unwrap_or(u64::MAX)
        .checked_div(u64::try_from(outcome.fetches).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A per-second rate from a count and an elapsed time.
///
/// Integer throughout: the workspace denies `cast_precision_loss`, and a rate
/// is a reporting figure rather than something a test asserts on, so
/// microsecond resolution is ample. Zero elapsed reports zero rather than
/// dividing by it.
fn per_second(count: u64, elapsed: Duration) -> u64 {
    count
        .saturating_mul(1_000_000)
        .checked_div(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// One row per profile, so the mechanism is comparable across them at a glance.
///
/// Criterion reports time; this reports what the pipeline did to earn it. Three
/// pairs of columns carry most of the meaning. `pool` against `peak` says
/// whether a run was bounded by connections or by something else. `per read` and
/// `served/f` are the coalescing ratio measured from the backend and from the
/// scheduler. And `resident` against the residency limit says whether memory
/// stayed inside its bound while all of that happened.
fn print_table(probed: &[(FanOutProfile, Outcome)]) {
    const HEADER: &str = "profile";
    println!();
    println!(
        "{HEADER:<22} {:>6} {:>7} {:>5} {:>5} {:>7} {:>11} {:>8} {:>12} {:>7} {:>9} {:>8} {:>9} {:>9} {:>8} {:>6} {:>7} {:>10} {:>11} {:>6}",
        "parts",
        "per grp",
        "pool",
        "peak",
        "ms",
        "deliv/s",
        "reads/s",
        "deliveries",
        "reads",
        "per read",
        "fetches",
        "served/f",
        "empty",
        "deferred",
        "suppr",
        "rounds",
        "resident",
        "reclaimed",
        "starv",
    );
    println!("{}", "-".repeat(205));
    for (profile, outcome) in probed {
        println!(
            "{:<22} {:>6} {:>7} {:>5} {:>5} {:>7} {:>11} {:>8} {:>12} {:>7} {:>9} {:>8} {:>9} {:>9} {:>8} {:>6} {:>7} {:>10} {:>11} {:>6}",
            profile.name,
            profile.partitions(),
            profile.partitions_per_group(),
            profile.pool_permits,
            outcome.peak_in_flight,
            outcome.elapsed.as_millis(),
            per_second(outcome.delivered, outcome.elapsed),
            per_second(
                u64::try_from(outcome.reads).unwrap_or(u64::MAX),
                outcome.elapsed
            ),
            outcome.delivered,
            outcome.reads,
            per_read(outcome),
            outcome.fetches,
            served_per_fetch(outcome),
            outcome.empty_reads,
            outcome.deferred,
            outcome.suppressed,
            outcome.rounds,
            outcome.peak_resident_bytes,
            outcome.reclaimed_events,
            outcome.worst_starvation,
        );
    }
    println!(
        "\n`deliv/s` counts deliveries - readers x events - because that is the \
         work the fan-out actually does; a rate over unique events would climb \
         with reader count on its own and make these rows incomparable. \
         `reads/s` is the load the same run puts on storage."
    );
    println!(
        "residency limit {} events x {} bytes an event; `empty` counts fetches \
         whose tail had not been assigned a sequence yet",
        probed
            .first()
            .map_or(0, |(profile, _)| profile.residency_events),
        footprint_of_one_event(),
    );
    println!();
}

/// Times one profile, re-verifying every iteration.
fn measure(c: &mut Criterion, runtime: &Runtime, profile: FanOutProfile, expected: u64) {
    let mut group = c.benchmark_group("delivery");
    group.sample_size(profile.criterion.samples);
    group.measurement_time(profile.criterion.measurement);
    group.warm_up_time(profile.criterion.warmup);
    group.throughput(Throughput::Elements(expected));
    group.bench_function(profile.name, |b| {
        b.iter(|| {
            let outcome = runtime.block_on(run_once(profile));
            verify(&outcome, profile);
            outcome.delivered
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

const VALIDATION_CRITERION: CriterionSettings = CriterionSettings {
    samples: 10,
    measurement: Duration::from_secs(10),
    warmup: Duration::from_secs(1),
};

const RECLAIMING: BackgroundJobs = BackgroundJobs {
    reclaim: true,
    reclaim_interval: Duration::from_millis(1),
};

/// A profile with the knobs most runs share, to be adjusted per case.
const fn baseline(name: &'static str, tier: Tier, pattern: ReaderPattern) -> FanOutProfile {
    FanOutProfile {
        name,
        tier,
        topics: 4,
        partitions_per_topic: 8,
        groups: 64,
        topics_per_group: 2,
        // Four times the residency, so reclamation is not optional and the
        // memory bound is under test rather than incidentally satisfied.
        events_per_partition: 1024,
        reader_pattern: pattern,
        // Comfortably above the partition count, because a round issues at most
        // one fetch per partition and so can never want more than that many
        // connections. Every profile but the deliberately choked one runs with
        // room to spare: if the pool were the constraint everywhere, none of
        // them would be measuring what they claim to - a reader pattern's cost
        // would be indistinguishable from waiting for a connection.
        pool_permits: 48,
        pool_expectation: PoolExpectation::Headroom,
        // Smaller than the residency, so a single fetch cannot breach the limit
        // on its own and the interesting pressure comes from accumulation.
        fetch_max_events: 64,
        starvation_weight: StarvationWeight(10),
        backend_latency: Duration::from_micros(200),
        visibility_gap: Duration::ZERO,
        residency_events: 256,
        gap_threshold_events: 256,
        background: RECLAIMING,
        min_deliveries_per_read: 64,
        // Tight enough to guard. Runs settle at a handful of scans, so a bound
        // in the thousands would have permitted anything.
        max_starvation: 64,
        criterion: VALIDATION_CRITERION,
    }
}

fn profiles() -> Vec<FanOutProfile> {
    let mut all = vec![
        // The one profile that is pool-bound, and the only one whose pool fills.
        // Identical to `tail_64g` in every other respect, so the pair is a
        // controlled experiment: read counts barely move between them while wall
        // time does, because coalescing sets the fetch count from partitions and
        // reader clusters. The pool bounds latency, not the work sent to storage.
        FanOutProfile {
            pool_permits: 2,
            pool_expectation: PoolExpectation::Saturated,
            ..baseline("pool_choked_2c", Tier::Validation, ReaderPattern::Tail)
        },
        // Every group at the tail: one cluster a partition, so one fetch serves
        // all sixty-four. The case the design is built for, and the comparison
        // point for the choked profile above.
        baseline("tail_64g", Tier::Validation, ReaderPattern::Tail),
        // Everything already persisted, everyone replaying from zero.
        baseline("sweep_64g", Tier::Validation, ReaderPattern::Sweep),
        // Several clusters a partition, so the loader must choose.
        FanOutProfile {
            min_deliveries_per_read: 8,
            ..baseline("dispersed_64g", Tier::Validation, ReaderPattern::Dispersed)
        },
        // A laggard cannot be given bounded residency, so it pays in refetches.
        FanOutProfile {
            min_deliveries_per_read: 8,
            ..baseline(
                "laggards_64g",
                Tier::Validation,
                ReaderPattern::TailWithLaggards,
            )
        },
        // Appended events are not readable for a while, so the tail poller is
        // what keeps readers moving rather than dead code.
        FanOutProfile {
            visibility_gap: Duration::from_millis(2),
            min_deliveries_per_read: 8,
            ..baseline("late_visibility_64g", Tier::Validation, ReaderPattern::Tail)
        },
        // The capacity design point: a thousand groups, one topic each, sixteen
        // partitions, against sixteen shared connections.
        FanOutProfile {
            topics: 4,
            partitions_per_topic: 16,
            groups: 1000,
            topics_per_group: 1,
            events_per_partition: 1024,
            // Sixty-four partitions against sixteen connections: the real
            // deployment ratio, which is pool-bound by construction. It lives on
            // its own tier, so the default run still has exactly one profile
            // whose pool fills.
            pool_permits: 16,
            pool_expectation: PoolExpectation::Saturated,
            min_deliveries_per_read: 1000,
            ..baseline("tail_1000g", Tier::Longhaul, ReaderPattern::Tail)
        },
        // A residency limit a fraction of the stream, so the bound is what the
        // run is actually testing: sixteen turnovers of the resident set while
        // every reader still receives every event.
        FanOutProfile {
            events_per_partition: 4096,
            residency_events: 256,
            gap_threshold_events: 256,
            min_deliveries_per_read: 8,
            ..baseline("memory_bound_64g", Tier::Validation, ReaderPattern::Tail)
        },
        // Dispersed readers, a residency limit far below the stream, and a
        // backend that lags its own notifications.
        FanOutProfile {
            groups: 256,
            events_per_partition: 4096,
            residency_events: 512,
            gap_threshold_events: 512,
            visibility_gap: Duration::from_millis(1),
            min_deliveries_per_read: 4,
            ..baseline("stress_dispersed", Tier::Stress, ReaderPattern::Dispersed)
        },
    ];
    all.retain(|profile| match profile.tier {
        Tier::Validation => true,
        Tier::Longhaul => std::env::var("BENCH_LONGHAUL").is_ok(),
        Tier::Stress => std::env::var("BENCH_STRESS").is_ok(),
    });
    all
}

fn delivery(c: &mut Criterion) {
    let runtime = Runtime::new().expect("tokio runtime");

    // Every profile is probed before any is measured, so the table below is a
    // single comparable picture rather than numbers scattered through
    // criterion's output.
    let probed: Vec<(FanOutProfile, Outcome)> = profiles()
        .into_iter()
        .map(|profile| (profile, probe(&runtime, profile)))
        .collect();

    // Printed *before* verification, deliberately. The table is the diagnostic
    // output, so a profile that violates an invariant is exactly when it is
    // wanted - suppressing it by asserting first meant a failure told you which
    // bound broke and nothing about why.
    print_table(&probed);

    for (profile, outcome) in &probed {
        verify(outcome, *profile);
    }

    for (profile, outcome) in &probed {
        measure(c, &runtime, *profile, outcome.expected);
    }
}

criterion_group!(benches, delivery);
criterion_main!(benches);
