//! Whether reclamation actually runs, and holds, over a long stream.
//!
//! A unit test can show that one reclamation pass takes the right segments. It
//! cannot show that residency stays bounded while a hundred thousand events go
//! through a cache that can hold eight thousand, which is the claim the design
//! rests on. These are simulations of that: in-process, deterministic, no
//! runtime, no storage.
//!
//! Three assertions do three different jobs, and they are not interchangeable.
//! The flow identity proves the accounting is consistent - it holds even when
//! nothing is ever reclaimed, so it is not evidence that eviction ran. The
//! turnover ratio is that evidence: it can only be met if the resident set was
//! recycled many times over. The peak bound proves the limit held, and is
//! vacuously true of a run that never filled the cache, which is why it travels
//! with the turnover ratio rather than alone.
//!
//! `reclamation_disabled_fails_the_proof` is what keeps the other tests
//! honest - if the checks passed with reclamation switched off, they would be
//! measuring nothing.

use chrono::Utc;
use serde_json::json;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::model::{Event, Sequence};

use super::cache::{AbsorbedFetch, PartitionCache, ReaderHandle};
use super::reclaim::{GapThresholdEvents, ReclaimPolicy, ResidencyLimitBytes};
use super::segment::Segment;
use crate::domain::streaming::read::{MaxBytes, MaxEvents, PartitionRead, ReadLimit};
use std::sync::Arc;

/// Events per fetch, and so events per resident segment.
const BATCH: i64 = 256;
/// Events the cache is sized to hold, which the stream exceeds many times over.
const CAP_EVENTS: usize = 8192;
/// Total streamed. Chosen so the turnover ratio clears its threshold with
/// margin: 120000 against a peak of roughly 8448 is about 14, asserted at 10.
const TOTAL: i64 = 120_000;
const MIN_TURNOVER: u64 = 10;
/// A stuck reader would otherwise spin forever; finishing inside this is itself
/// part of what is being checked.
const MAX_TICKS: usize = 20_000;
/// Where the non-laggard readers start in the laggard pattern - seven eighths
/// of the way through, so the laggards are a long way behind them.
const TAIL_START: i64 = 105_000;

fn event(sequence: i64) -> Event {
    let sequence = Sequence::assigned(sequence);
    Event {
        id: Uuid::nil(),
        r#type: crate::test_support::event_type_id(
            "gts.cf.core.events.event.v1~x.eb.o.created.v1~",
        ),
        topic: GtsInstanceId::try_new("gts.cf.core.events.topic.v1~x.eb.orders.acme.v1")
            .expect("static gts id is valid"),
        tenant_id: Uuid::nil(),
        source: "residency".to_owned(),
        subject: "order".to_owned(),
        subject_type: "order".to_owned(),
        occurred_at: Utc::now(),
        trace_parent: None,
        data: json!({ "n": sequence, "body": "some representative payload" }),
        meta: None,
        partition: Some(0),
        sequence: Some(sequence),
        sequence_time: None,
    }
}

/// One event's measured footprint, so the byte limit is expressed in events
/// rather than in a magic number that drifts when the event shape changes.
fn footprint_of_one_event() -> usize {
    Segment::builder()
        .from(Sequence::assigned(1))
        .through(Sequence::assigned(1))
        .events(vec![event(1)])
        .build()
        .bytes()
}

fn policy_holding(cap_events: usize) -> ReclaimPolicy {
    ReclaimPolicy::new(
        GapThresholdEvents(CAP_EVENTS),
        ResidencyLimitBytes(cap_events.saturating_mul(footprint_of_one_event())),
    )
}

fn absorb_span(cache: &PartitionCache, from: i64, through: i64) {
    let segment = Segment::builder()
        .from(Sequence::assigned(from))
        .through(Sequence::assigned(through))
        .events((from..=through).map(event).collect())
        .build();
    cache.absorb(AbsorbedFetch::builder(segment).build());
}

fn read_limit() -> ReadLimit {
    ReadLimit::new(
        MaxEvents(usize::try_from(BATCH).unwrap_or(usize::MAX)),
        MaxBytes(1024 * 1024),
    )
}

/// Deterministic pseudo-random offsets. Seeded by a constant so a failure is
/// reproducible - the reference implementation's dispersed-reader test uses an
/// unseeded generator and cannot replay one.
struct Spread(u64);

impl Spread {
    fn next_below(&mut self, bound: i64) -> i64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let bound = u64::try_from(bound).unwrap_or(1).max(1);
        i64::try_from((self.0 >> 33) % bound).unwrap_or(0)
    }
}

/// How readers are positioned when the run starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pattern {
    /// Every reader tracks the producer.
    Tail,
    /// Every reader sweeps the whole stream from the beginning, which is where
    /// reclamation has to keep up with readers rather than with the producer.
    Sweep,
    /// Readers scattered across the stream, which is what makes several
    /// clusters and exercises the gap rule.
    Dispersed,
    /// Most readers at the tail, a few far behind, which is the case where
    /// bounded residency is not something the design can promise.
    TailWithLaggards,
}

struct Outcome {
    refetches: usize,
    ticks: usize,
}

/// Runs one pattern to completion and returns what it cost.
///
/// A reader answered `Unknown` refetches, exactly as the loader would - so the
/// refetch traffic reclamation causes is measured here rather than assumed.
fn run(
    cache: &Arc<PartitionCache>,
    readers: usize,
    pattern: Pattern,
    reclaim_every: usize,
) -> Outcome {
    let mut spread = Spread(0x5eed);
    let mut produced: i64 = 0;

    // Everything is already persisted for a sweep; otherwise the producer runs
    // alongside the readers.
    if pattern == Pattern::Sweep {
        while produced < TOTAL {
            let from = produced.saturating_add(1);
            let through = from.saturating_add(BATCH - 1).min(TOTAL);
            absorb_span(cache, from, through);
            produced = through;
        }
    }

    let handles: Vec<ReaderHandle> = (0..readers)
        .map(|index| {
            let start = match pattern {
                Pattern::Tail | Pattern::Sweep => 0,
                Pattern::Dispersed => spread.next_below(TOTAL),
                Pattern::TailWithLaggards => {
                    if index % 16 == 0 {
                        0
                    } else {
                        TAIL_START
                    }
                }
            };
            cache.track_reader(Sequence::assigned(start))
        })
        .collect();

    let mut refetches = 0;
    let mut ticks = 0;

    while ticks < MAX_TICKS {
        ticks += 1;

        if produced < TOTAL {
            let from = produced.saturating_add(1);
            let through = from.saturating_add(BATCH - 1).min(TOTAL);
            absorb_span(cache, from, through);
            produced = through;
        }

        for (index, handle) in handles.iter().enumerate() {
            // Laggards read on one tick in eight, which is what makes them lag.
            if pattern == Pattern::TailWithLaggards && index % 16 == 0 && ticks % 8 != 0 {
                continue;
            }

            let offset = handle.offset().as_i64();
            if offset >= TOTAL {
                continue;
            }
            match handle.read(read_limit()) {
                // A hit needs nothing done: `read` advanced the handle
                // itself. A quiet tail needs nothing done either - the two
                // arms are deliberately one here, unlike `Unknown` below,
                // which is the only outcome this loop reacts to.
                PartitionRead::Hit { .. } | PartitionRead::NothingNew => {}
                PartitionRead::Unknown => {
                    let from = offset.saturating_add(1);
                    let through = from.saturating_add(BATCH - 1).min(produced);
                    if through >= from {
                        absorb_span(cache, from, through);
                        refetches += 1;
                    }
                }
            }
        }

        if ticks % reclaim_every == 0 {
            let _ = cache.reclaim();
        }

        if handles
            .iter()
            .all(|handle| handle.offset().as_i64() >= TOTAL)
        {
            break;
        }
    }

    Outcome { refetches, ticks }
}

/// The three assertions, applied to a run that was supposed to reclaim.
fn assert_reclamation_held(cache: &PartitionCache, outcome: &Outcome, label: &str) {
    let stats = cache.stats();

    assert!(
        stats.balances(),
        "{label}: every accounted byte must be resident or reclaimed - \
         absorbed {}, reclaimed {}, resident {}",
        stats.absorbed().bytes(),
        stats.reclaimed().bytes(),
        stats.resident().bytes()
    );

    assert!(
        stats.reclaimed().events() >= stats.peak().events().saturating_mul(MIN_TURNOVER),
        "{label}: the resident set must have been recycled at least \
         {MIN_TURNOVER} times over - reclaimed {} events against a peak of {}",
        stats.reclaimed().events(),
        stats.peak().events()
    );

    assert!(
        stats.freeing_passes() > 0,
        "{label}: no pass freed anything, so nothing was being reclaimed"
    );
    assert!(
        stats.passes() >= stats.freeing_passes(),
        "{label}: a freeing pass is a pass"
    );

    assert!(
        outcome.ticks < MAX_TICKS,
        "{label}: run did not finish - a reader is stuck refetching a span \
         that is reclaimed before it can read it"
    );
}

#[test]
fn readers_at_the_tail_keep_residency_bounded() {
    let cap = policy_holding(CAP_EVENTS);
    let cache = PartitionCache::with_reclaim_policy(cap);

    let outcome = run(&cache, 64, Pattern::Tail, 4);

    assert_reclamation_held(&cache, &outcome, "tail");
    let stats = cache.stats();
    let one_batch = usize::try_from(BATCH).unwrap_or(0) * footprint_of_one_event();
    let allowed =
        u64::try_from(cap.residency_limit_bytes().saturating_add(one_batch)).unwrap_or(u64::MAX);
    assert!(
        stats.peak().bytes() <= allowed,
        "peak {} must not exceed the limit by more than the batch that \
         breached it, since a batch is absorbed before it is trimmed",
        stats.peak().bytes()
    );
}

#[test]
fn readers_sweeping_from_the_beginning_keep_residency_bounded() {
    let cache = PartitionCache::with_reclaim_policy(policy_holding(CAP_EVENTS));

    // The whole stream is persisted before anyone reads, so reclamation has to
    // track the readers rather than the producer. This is the pattern the
    // deleted merge would have failed outright: one span grown across the
    // entire stream is reclaimable only once every reader has finished.
    let outcome = run(&cache, 64, Pattern::Sweep, 4);

    assert_reclamation_held(&cache, &outcome, "sweep");
}

#[test]
fn readers_dispersed_across_the_stream_keep_residency_bounded() {
    let cache = PartitionCache::with_reclaim_policy(policy_holding(CAP_EVENTS));

    let outcome = run(&cache, 64, Pattern::Dispersed, 4);

    assert_reclamation_held(&cache, &outcome, "dispersed");
}

#[test]
fn a_laggard_costs_refetches_rather_than_unbounded_residency() {
    let cache = PartitionCache::with_reclaim_policy(policy_holding(CAP_EVENTS));

    let outcome = run(&cache, 64, Pattern::TailWithLaggards, 4);

    assert_reclamation_held(&cache, &outcome, "laggards");
    // The obligation here is deliberately different. Holding everything between
    // a laggard and the tail is not something the design offers, so the laggard
    // pays in refetches instead - and that the refetches happen is the evidence
    // the gap rule fired rather than residency quietly growing.
    assert!(
        outcome.refetches > 0,
        "a laggard behind the reclaimed frontier must be refetching"
    );
}

#[test]
fn reclamation_disabled_fails_the_proof() {
    // Nothing is ever outside a window, nothing is ever over the limit, and no
    // pass is ever run.
    let cache = PartitionCache::with_reclaim_policy(ReclaimPolicy::new(
        GapThresholdEvents(usize::MAX),
        ResidencyLimitBytes(usize::MAX),
    ));

    let outcome = run(&cache, 8, Pattern::Tail, usize::MAX);
    let stats = cache.stats();

    // The identity still holds, which is exactly why it is not the proof: with
    // nothing reclaimed, everything absorbed is simply still resident.
    assert!(stats.balances());
    assert_eq!(
        stats.reclaimed().events(),
        0,
        "no pass was run and the limit was never breached, so nothing should \
         have been taken"
    );
    assert_eq!(stats.freeing_passes(), 0);

    // These two are the proof, and both fail as they must.
    assert!(
        stats.reclaimed().events() < stats.peak().events().saturating_mul(MIN_TURNOVER),
        "turnover must fail when nothing is reclaimed"
    );
    assert!(
        stats.resident().events() > u64::try_from(CAP_EVENTS).unwrap_or(u64::MAX),
        "residency must run past the cap when nothing is reclaimed - it \
         reached {} events",
        stats.resident().events()
    );
    assert_eq!(
        outcome.refetches, 0,
        "nothing was reclaimed, so nothing needed refetching"
    );
}
