//! The cache under genuine concurrency: many readers reading while many writers
//! absorb and a maintenance thread reclaims underneath both.
//!
//! Real OS threads rather than a runtime, because every entry point on
//! `PartitionCache` is synchronous - `read_from`, `absorb` and `reclaim` take
//! `&self` and take their own locks. What these tests can see and the
//! simulations in `residency_tests` cannot: contention on the segment lock,
//! exclusion between an absorb and a read, contention on the reader registry,
//! and the wake path.
//!
//! Every reader verifies **every sequence it is handed**, in order, exactly
//! once. That is the assertion that matters and the one the simulations were
//! missing: they advanced readers on `accounted_through` and threw the events
//! away, so a reader that silently skipped a span still passed. Under a bounded
//! read limit, advancing past an event that was withheld shows up here as a
//! sequence arriving out of order.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use chrono::Utc;
use serde_json::json;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::model::{Event, Sequence};

use super::cache::{AbsorbedFetch, CacheRead, PartitionCache};
use super::reclaim::{GapThresholdEvents, ReclaimPolicy, ResidencyLimitBytes};
use super::segment::{MaxBytes, MaxEvents, ReadLimit, Segment};

const BATCH: Sequence = 256;
/// Total streamed per partition. Smaller than the single-threaded simulations
/// because every delivered event is checked by hand here.
const TOTAL: Sequence = 20_000;
/// Events the cache is sized to hold, so the stream turns it over many times.
const CAP_EVENTS: usize = 4096;
/// Logical readers, multiplexed over a realistic number of threads. Spawning
/// one thread each would measure the scheduler rather than the cache.
const READERS: usize = 256;
const READER_THREADS: usize = 16;
/// Concurrent absorbers, standing in for a connection pool.
const WRITER_THREADS: usize = 8;
/// A stuck reader must fail rather than hang the suite.
const MAX_SPINS: usize = 5_000;
/// How often the maintenance thread runs, matching the reference
/// implementation's one-millisecond tick rather than spinning.
const RECLAIM_INTERVAL: Duration = Duration::from_millis(1);

fn event(sequence: Sequence) -> Event {
    Event {
        id: Uuid::nil(),
        r#type: GtsInstanceId::try_new("gts.cf.core.events.event_type.v1~x.eb.o.created.v1")
            .expect("static gts id is valid"),
        topic: GtsInstanceId::try_new("gts.cf.core.events.topic.v1~x.eb.orders.acme.v1")
            .expect("static gts id is valid"),
        partition_key: None,
        tenant_id: Uuid::nil(),
        source: "concurrency".to_owned(),
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

fn footprint_of_one_event() -> usize {
    Segment::builder()
        .from(1)
        .through(1)
        .events(vec![event(1)])
        .build()
        .bytes()
}

fn policy() -> ReclaimPolicy {
    ReclaimPolicy::new(
        GapThresholdEvents(CAP_EVENTS),
        ResidencyLimitBytes(CAP_EVENTS.saturating_mul(footprint_of_one_event())),
    )
}

fn absorb_span(cache: &PartitionCache, from: Sequence, through: Sequence) {
    let segment = Segment::builder()
        .from(from)
        .through(through)
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

/// One logical reader's progress: the next sequence it must be handed.
///
/// Held per reader rather than derived from the handle's offset, because the
/// handle's offset is what the *cache* told it and this is what the reader has
/// actually seen. Comparing the two is the point.
struct Progress {
    expected_next: Sequence,
}

/// Drives one logical reader one step. Returns whether it made progress.
///
/// Panics on any out-of-order or duplicated sequence, which is what makes this
/// a correctness test rather than a load generator.
fn step(cache: &PartitionCache, handle: &super::cache::ReaderHandle, progress: &mut Progress) {
    let offset = handle.offset();
    match cache.read_from(offset, read_limit()) {
        CacheRead::Hit {
            events,
            accounted_through,
        } => {
            for delivered in events.iter() {
                let sequence = delivered
                    .sequence
                    .expect("a cached event carries its sequence");
                assert_eq!(
                    sequence, progress.expected_next,
                    "reader at offset {offset} was handed {sequence} while it \
                     was owed {} - a sequence was skipped or repeated",
                    progress.expected_next
                );
                progress.expected_next = progress.expected_next.saturating_add(1);
            }
            handle.advance(accounted_through);
        }
        CacheRead::NothingNew => thread::yield_now(),
        CacheRead::Unknown { .. } => {
            // What the loader does on a miss: fetch from where the reader is.
            // Reclamation may have taken this span, or a concurrent writer may
            // not have produced it yet; either way the demand is the same.
            let from = offset.saturating_add(1);
            let through = from.saturating_add(BATCH - 1).min(TOTAL);
            if through >= from {
                absorb_span(cache, from, through);
            } else {
                thread::yield_now();
            }
        }
    }
}

#[test]
fn parallel_readers_and_writers_lose_no_events() {
    let cache = PartitionCache::with_reclaim_policy(policy());
    let handles: Vec<super::cache::ReaderHandle> =
        (0..READERS).map(|_| cache.track_reader(0)).collect();

    // Writers claim disjoint spans from one counter, which is how a pool of
    // connections divides a partition's backlog: the claim is atomic, the
    // fetches complete in whatever order they finish.
    let next_span = AtomicI64::new(1);
    let done = AtomicBool::new(false);
    // Readers, writers and the reclaimer all start together. Without this the
    // first threads spawned run unopposed and the contention window never
    // fully overlaps.
    let gate = Barrier::new(READER_THREADS + WRITER_THREADS + 1);

    // Shared references taken before the scope, so every closure can be `move`
    // and copy them rather than borrowing the locals themselves.
    let cache = &cache;
    let next_span = &next_span;
    let done = &done;
    let gate = &gate;

    thread::scope(|scope| {
        for _ in 0..WRITER_THREADS {
            scope.spawn(move || {
                eprintln!("W arrive");
                gate.wait();
                eprintln!("W go");
                loop {
                    let from = next_span.fetch_add(BATCH, Ordering::Relaxed);
                    if from > TOTAL {
                        eprintln!("W done");
                        return;
                    }
                    absorb_span(&cache, from, from.saturating_add(BATCH - 1).min(TOTAL));
                }
            });
        }

        // Kept so it can be stopped and joined *inside* the scope: exiting the
        // scope joins every spawned thread, so a thread waiting on a flag set
        // after the scope would never be able to finish.
        let reclaimer = scope.spawn(move || {
            eprintln!("M arrive");
            gate.wait();
            eprintln!("M go");
            while !done.load(Ordering::Relaxed) {
                let _ = cache.reclaim();
                // Throttled deliberately. An unthrottled pass holds the write
                // lock back-to-back and starves every reader behind it, which
                // is a property of the test harness rather than of the cache -
                // the reference implementation ticks maintenance once a
                // millisecond and production far slower still.
                thread::sleep(RECLAIM_INTERVAL);
            }
        });

        let mut chunks: Vec<Vec<&super::cache::ReaderHandle>> =
            (0..READER_THREADS).map(|_| Vec::new()).collect();
        for (index, handle) in handles.iter().enumerate() {
            if let Some(chunk) = chunks.get_mut(index % READER_THREADS) {
                chunk.push(handle);
            }
        }

        let readers: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                scope.spawn(move || {
                eprintln!("R arrive");
                gate.wait();
                eprintln!("R go");
                let mut progress: Vec<Progress> = chunk
                    .iter()
                    .map(|_| Progress { expected_next: 1 })
                    .collect();
                let mut spins = 0;

                while spins < MAX_SPINS {
                    let mut finished = true;
                    for (handle, state) in chunk.iter().zip(progress.iter_mut()) {
                        if state.expected_next > TOTAL {
                            continue;
                        }
                        finished = false;
                        step(&cache, handle, state);
                    }
                    if finished {
                        return;
                    }
                    spins += 1;
                    if spins % 1_000 == 0 {
                        let owed: Vec<Sequence> =
                            progress.iter().map(|state| state.expected_next).collect();
                        eprintln!(
                            "DIAG spins={spins} owed={owed:?} segments={} newest={} resident={}",
                            cache.segment_count(),
                            cache.newest_accounted(),
                            cache.stats().resident().events()
                        );
                    }
                }
                panic!("readers made no progress within {MAX_SPINS} rounds");
                })
            })
            .collect();

        for reader in readers {
            assert!(reader.join().is_ok(), "a reader thread failed");
        }
        done.store(true, Ordering::Relaxed);
        assert!(reclaimer.join().is_ok(), "the maintenance thread failed");
    });

    // Every reader saw every sequence exactly once, in order - checked in
    // `step`. All that is left is that they all got to the end.
    for handle in &handles {
        assert!(
            handle.offset() >= TOTAL,
            "a reader stopped at {} short of {TOTAL}",
            handle.offset()
        );
    }

    let stats = cache.stats();
    assert!(
        stats.balances(),
        "the flow identity must survive concurrency"
    );
    assert!(
        stats.reclaimed().events() > 0,
        "reclamation ran alongside the readers, so it must have taken something"
    );
}

#[test]
fn concurrent_absorbs_keep_the_spans_disjoint() {
    let cache = PartitionCache::with_reclaim_policy(ReclaimPolicy::new(
        GapThresholdEvents(usize::MAX),
        ResidencyLimitBytes(usize::MAX),
    ));
    let next_span = AtomicI64::new(1);
    let gate = Barrier::new(WRITER_THREADS);
    let cache = &cache;
    let next_span = &next_span;
    let gate = &gate;

    // Writers deliberately overlap: each claims a span but extends it past its
    // own end, so neighbouring fetches collide the way two readers' demands
    // would. Narrowing has to keep the map disjoint under that.
    thread::scope(|scope| {
        for _ in 0..WRITER_THREADS {
            scope.spawn(move || {
                gate.wait();
                loop {
                    let from = next_span.fetch_add(BATCH, Ordering::Relaxed);
                    if from > TOTAL {
                        return;
                    }
                    let overlapping = from.saturating_add(BATCH).saturating_add(BATCH - 1);
                    absorb_span(&cache, from, overlapping.min(TOTAL));
                }
            });
        }
    });

    let spans = cache.spans();
    for pair in spans.windows(2) {
        if let [(_, left_through), (right_from, _)] = pair {
            assert!(
                left_through < right_from,
                "spans {left_through} and {right_from} overlap after concurrent \
                 absorbs"
            );
        }
    }
    assert!(cache.stats().balances());
}

#[test]
fn a_wake_reaches_every_parked_reader() {
    let cache = PartitionCache::with_reclaim_policy(policy());
    let handles: Vec<super::cache::ReaderHandle> =
        (0..READERS).map(|_| cache.track_reader(0)).collect();
    let woken = Arc::new(AtomicI64::new(0));

    // Everyone parks on an empty cache, then one absorb has to satisfy all of
    // them. This is the wake storm at the design point: one publish, a thousand
    // readers.
    let gate = Barrier::new(READER_THREADS + 1);
    let cache = &cache;
    let gate = &gate;
    thread::scope(|scope| {
        let mut chunks: Vec<Vec<&super::cache::ReaderHandle>> =
            (0..READER_THREADS).map(|_| Vec::new()).collect();
        for (index, handle) in handles.iter().enumerate() {
            if let Some(chunk) = chunks.get_mut(index % READER_THREADS) {
                chunk.push(handle);
            }
        }

        for chunk in chunks {
            let woken = Arc::clone(&woken);
            scope.spawn(move || {
                gate.wait();
                for handle in chunk {
                    let mut spins = 0;
                    while !handle.has_data() && spins < MAX_SPINS {
                        spins += 1;
                        thread::yield_now();
                    }
                    assert!(handle.has_data(), "a reader was never woken");
                    woken.fetch_add(1, Ordering::Relaxed);
                }
            });
        }

        gate.wait();
        absorb_span(&cache, 1, BATCH);
    });

    assert_eq!(
        woken.load(Ordering::Relaxed),
        i64::try_from(READERS).unwrap_or(i64::MAX),
        "one absorb must satisfy every reader parked behind it"
    );
}
