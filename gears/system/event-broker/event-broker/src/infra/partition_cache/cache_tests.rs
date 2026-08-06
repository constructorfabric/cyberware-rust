//! Segment-map behaviour. Everything except the two `wait` tests is
//! synchronous: `watch::channel` constructs without a runtime, so readiness is
//! testable without one.

use chrono::Utc;
use serde_json::json;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::model::{Event, Sequence};

use super::cache::{AbsorbedFetch, PartitionCache};
use super::reclaim::{GapThresholdEvents, ReclaimPolicy, ResidencyLimitBytes};
use super::segment::Segment;
use crate::domain::streaming::read::{MaxBytes, MaxEvents, PartitionRead, ReadLimit};

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

/// Absorbs a fetch that accounted for `from..=through` and found `present`.
fn absorb(cache: &PartitionCache, from: i64, through: i64, present: &[i64]) {
    let segment = Segment::builder()
        .from(Sequence::assigned(from))
        .through(Sequence::assigned(through))
        .events(present.iter().copied().map(event).collect())
        .build();
    cache.absorb(AbsorbedFetch::builder(segment).build());
}

fn read(cache: &PartitionCache, offset: i64) -> PartitionRead {
    cache.read_from(Sequence::assigned(offset), ReadLimit::unbounded())
}

/// The fixtures here number events plainly; these two carry the numbers across
/// the type boundary so each assertion reads as a list of events rather than a
/// column of constructor calls.
fn hit_sequences(result: &PartitionRead) -> Vec<i64> {
    match result {
        PartitionRead::Hit { events, .. } => events
            .iter()
            .filter_map(|e| e.sequence)
            .map(Sequence::as_i64)
            .collect(),
        _ => vec![],
    }
}

fn spans(cache: &PartitionCache) -> Vec<(i64, i64)> {
    cache
        .spans()
        .into_iter()
        .map(|(from, through)| (from.as_i64(), through.as_i64()))
        .collect()
}

#[test]
fn an_empty_cache_accounts_for_nothing() {
    let cache = PartitionCache::new();

    assert!(matches!(read(&cache, 0), PartitionRead::Unknown));
}

#[test]
fn a_resident_span_serves_a_read_inside_it() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 104, &[100, 101, 102, 103, 104]);

    assert_eq!(hit_sequences(&read(&cache, 101)), vec![102, 103, 104]);
}

#[test]
fn a_position_past_the_span_is_the_tail_not_a_gap() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 104, &[100, 101, 102, 103, 104]);

    assert!(matches!(read(&cache, 104), PartitionRead::NothingNew));
}

#[test]
fn a_position_below_every_span_is_unknown_and_names_the_next_span() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 104, &[100]);

    // Position 50 is unaccounted for. The reader is told only that it cannot be
    // answered here; what to fetch is the loader's to derive.
    assert!(matches!(read(&cache, 50), PartitionRead::Unknown));
}

#[test]
fn a_position_in_a_gap_between_spans_is_unknown_not_absent() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 150, &[100]);
    absorb(&cache, 300, 350, &[300]);

    // 200 lies between two accounted spans and in neither. Nothing has proven
    // it absent, so a reader there must wait rather than skip to 300.
    assert!(matches!(read(&cache, 200), PartitionRead::Unknown));
}

#[test]
fn a_hole_inside_an_accounted_span_is_stepped_over() {
    let cache = PartitionCache::new();
    // The fetch accounted for 100..=200 and found only two events; the rest
    // were deleted, and that is proven rather than unknown.
    absorb(&cache, 100, 200, &[100, 200]);

    assert_eq!(hit_sequences(&read(&cache, 100)), vec![200]);
    assert_eq!(hit_sequences(&read(&cache, 150)), vec![200]);
}

#[test]
fn an_accounted_span_emptied_by_deletion_still_serves_a_hit() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 200, &[100]);

    // Everything after 100 in the span is gone, but the span accounts for it,
    // so the reader may advance its frontier to 200 rather than stalling.
    match read(&cache, 100) {
        PartitionRead::Hit {
            events,
            accounted_through,
        } => {
            assert!(events.is_empty());
            assert_eq!(accounted_through, Sequence::assigned(200));
        }
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[test]
fn exactly_adjacent_spans_stay_separate_and_read_as_one() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 200, &[100]);
    absorb(&cache, 201, 300, &[250]);

    // Deliberately not merged: concatenating the storage would deep-copy every
    // resident event on every absorb, and a single grown span could only be
    // reclaimed once every reader had passed all of it.
    assert_eq!(cache.segment_count(), 2);
    assert_eq!(spans(&cache), vec![(100, 200), (201, 300)]);

    // Adjacency is derived instead, so one read crosses both.
    match read(&cache, 99) {
        PartitionRead::Hit {
            events,
            accounted_through,
        } => {
            assert_eq!(
                events
                    .iter()
                    .filter_map(|e| e.sequence)
                    .map(Sequence::as_i64)
                    .collect::<Vec<_>>(),
                vec![100, 250]
            );
            assert_eq!(accounted_through, Sequence::assigned(300));
            assert_eq!(events.runs(), 2, "one run borrowed from each segment");
        }
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[test]
fn a_read_crosses_adjacent_spans_whatever_holes_they_have() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 200, &[100]);
    absorb(&cache, 201, 300, &[300]);

    // Adjacency is about the accounted spans, not about density, so the walk
    // crosses two very sparse segments and steps over both their holes.
    assert_eq!(spans(&cache), vec![(100, 200), (201, 300)]);
    assert_eq!(hit_sequences(&read(&cache, 99)), vec![100, 300]);
}

#[test]
fn a_gap_between_spans_stops_the_walk() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 200, &[100]);
    absorb(&cache, 202, 300, &[250]);

    assert_eq!(cache.segment_count(), 2);
    assert_eq!(spans(&cache), vec![(100, 200), (202, 300)]);

    // 201 is unaccounted for. Crossing it would advance the reader over a
    // sequence nobody has established anything about, so the walk stops and
    // the reader is told only what the first span proved.
    match read(&cache, 99) {
        PartitionRead::Hit {
            events,
            accounted_through,
        } => {
            assert_eq!(
                events
                    .iter()
                    .filter_map(|e| e.sequence)
                    .map(Sequence::as_i64)
                    .collect::<Vec<_>>(),
                vec![100]
            );
            assert_eq!(
                accounted_through,
                Sequence::assigned(200),
                "not 250, and not 300"
            );
        }
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[test]
fn a_chain_of_adjacent_spans_reads_as_one() {
    let cache = PartitionCache::new();
    // Absorbed out of order, so this also proves the walk follows the map's
    // ordering rather than insertion order.
    absorb(&cache, 300, 399, &[300]);
    absorb(&cache, 100, 199, &[100]);
    absorb(&cache, 200, 299, &[200]);

    assert_eq!(spans(&cache), vec![(100, 199), (200, 299), (300, 399)]);

    match read(&cache, 99) {
        PartitionRead::Hit {
            events,
            accounted_through,
        } => {
            assert_eq!(
                events
                    .iter()
                    .filter_map(|e| e.sequence)
                    .map(Sequence::as_i64)
                    .collect::<Vec<_>>(),
                vec![100, 200, 300]
            );
            assert_eq!(accounted_through, Sequence::assigned(399));
            assert_eq!(events.runs(), 3);
        }
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[test]
fn absorbing_publishes_the_newest_accounted_sequence() {
    let cache = PartitionCache::new();
    assert_eq!(cache.newest_accounted(), Sequence::assigned(0));

    absorb(&cache, 100, 200, &[100]);

    assert_eq!(cache.newest_accounted(), Sequence::assigned(200));
}

#[test]
fn readiness_is_a_comparison_not_an_await() {
    let cache = PartitionCache::new();
    let reader = cache.track_reader(Sequence::assigned(99));
    assert!(!reader.has_data());

    absorb(&cache, 100, 200, &[100]);

    assert!(reader.has_data());
    reader.seek(Sequence::assigned(200));
    assert!(!reader.has_data());
}

#[tokio::test]
async fn a_wait_resolves_when_something_is_accounted_past_the_reader() {
    let cache = PartitionCache::new();
    let reader = cache.track_reader(Sequence::assigned(99));

    absorb(&cache, 100, 200, &[100]);

    // Already past the reader before the wait begins: level-triggered, so this
    // resolves rather than hanging for a subsequent change.
    reader.wait().await;
    assert!(reader.has_data());
}

#[tokio::test]
async fn an_append_between_a_check_and_a_wait_is_not_missed() {
    let cache = PartitionCache::new();
    let reader = cache.track_reader(Sequence::assigned(99));

    assert!(!reader.has_data());
    // The window an edge-triggered signal would lose.
    absorb(&cache, 100, 200, &[100]);
    reader.wait().await;

    assert!(reader.has_data());
}

#[test]
fn the_slowest_reader_is_the_loaders_runway_input() {
    let cache = PartitionCache::new();
    let fast = cache.track_reader(Sequence::assigned(500));
    let slow = cache.track_reader(Sequence::assigned(100));

    assert_eq!(cache.slowest_reader(), Some(Sequence::assigned(100)));
    assert_eq!(cache.reader_count(), 2);

    slow.seek(Sequence::assigned(600));
    assert_eq!(cache.slowest_reader(), Some(Sequence::assigned(500)));

    drop(fast);
    assert_eq!(cache.slowest_reader(), Some(Sequence::assigned(600)));
    assert_eq!(cache.reader_count(), 1);
}

#[test]
fn dropping_a_reader_deregisters_it() {
    let cache = PartitionCache::new();
    {
        let _reader = cache.track_reader(Sequence::assigned(100));
        assert_eq!(cache.reader_count(), 1);
    }

    assert_eq!(cache.reader_count(), 0);
    assert_eq!(cache.slowest_reader(), None);
}

#[test]
fn the_scanning_flag_is_reported_by_the_session_not_measured_here() {
    let cache = PartitionCache::new();
    let reader = cache.track_reader(Sequence::assigned(0));

    assert!(!reader.is_scanning());
    reader.report_scanning(true);
    assert!(reader.is_scanning());
}

#[test]
fn dead_spans_are_reclaimed() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 200, &[100, 150]);
    absorb(&cache, 300, 400, &[300]);
    let _reader = cache.track_reader(Sequence::assigned(200));
    assert_eq!(cache.segment_count(), 2);

    // Readers only move forward, so a span entirely at or below every reader
    // will never be read again.
    let report = cache.reclaim();

    assert_eq!(report.dead().segments(), 1);
    assert!(report.dead().bytes() > 0);
    assert!(report.gapped().is_empty(), "the span ahead is still wanted");
    assert_eq!(spans(&cache), vec![(300, 400)]);
}

#[test]
fn reclamation_does_not_wait_on_a_reader_holding_the_span() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 200, &[100, 150, 200]);

    // A reader is mid-read, holding the segment's storage.
    let held = match read(&cache, 99) {
        PartitionRead::Hit { events, .. } => events,
        other => panic!("expected Hit, got {other:?}"),
    };
    assert_eq!(
        cache.segment_holders(Sequence::assigned(100)),
        Some(2),
        "map plus the slice"
    );

    // Reclamation proceeds regardless - no pinning protocol to negotiate.
    let _reader = cache.track_reader(Sequence::assigned(200));
    cache.reclaim();
    assert_eq!(cache.segment_count(), 0);
    assert_eq!(cache.segment_holders(Sequence::assigned(100)), None);

    // And the holder reads out correctly from storage that is no longer in the
    // map, which is what makes reclaiming it safe rather than merely allowed.
    assert_eq!(
        held.iter()
            .filter_map(|e| e.sequence)
            .map(Sequence::as_i64)
            .collect::<Vec<_>>(),
        vec![100, 150, 200]
    );
}

#[test]
fn a_live_span_is_not_reclaimed() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 200, &[100]);
    let _reader = cache.track_reader(Sequence::assigned(150));

    // The span extends past the reader, so part of it is still wanted.
    let report = cache.reclaim();

    assert!(!report.freed_anything());
    assert_eq!(spans(&cache), vec![(100, 200)]);
}

#[test]
fn resident_bytes_tracks_what_is_held() {
    let cache = PartitionCache::new();
    assert_eq!(cache.resident_bytes(), 0);

    absorb(&cache, 100, 200, &[100, 150, 200]);
    let with_three = cache.resident_bytes();
    assert!(with_three > 0);

    let _reader = cache.track_reader(Sequence::assigned(200));
    cache.reclaim();

    assert_eq!(cache.resident_bytes(), 0);
}

#[test]
fn a_fetch_may_account_for_more_than_it_returned() {
    let cache = PartitionCache::new();
    // A fetch after 99 that returned only 200 proved 100..=199 absent.
    let segment = Segment::builder()
        .from(Sequence::assigned(200))
        .through(Sequence::assigned(200))
        .events(vec![event(200)])
        .build();
    cache.absorb(
        AbsorbedFetch::builder(segment)
            .accounted_from(Sequence::assigned(100))
            .accounted_through(Sequence::assigned(200))
            .build(),
    );

    assert_eq!(spans(&cache), vec![(100, 200)]);
    // A reader at 99 is now served rather than told the span is unknown.
    assert_eq!(hit_sequences(&read(&cache, 99)), vec![200]);
}

#[test]
fn a_bounded_read_does_not_advance_the_reader_past_what_it_delivered() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 109, &(100..=109).collect::<Vec<_>>());

    // Three events of a dense ten-event span. Reporting the span's `through`
    // here would tell the reader to advance to 109 and silently lose 103..=109.
    let read = cache.read_from(
        Sequence::assigned(99),
        ReadLimit::new(MaxEvents(3), MaxBytes(usize::MAX)),
    );

    match read {
        PartitionRead::Hit {
            events,
            accounted_through,
        } => {
            assert_eq!(
                events
                    .iter()
                    .filter_map(|e| e.sequence)
                    .map(Sequence::as_i64)
                    .collect::<Vec<_>>(),
                vec![100, 101, 102]
            );
            assert_eq!(accounted_through, Sequence::assigned(102));
        }
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[test]
fn a_limit_stopping_inside_a_later_segment_reports_only_what_it_delivered() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 102, &[100, 101, 102]);
    absorb(&cache, 103, 105, &[103, 104, 105]);

    // Four events: all of the first segment and one of the second. The reader
    // must be told 103, not the second segment's 105.
    let read = cache.read_from(
        Sequence::assigned(99),
        ReadLimit::new(MaxEvents(4), MaxBytes(usize::MAX)),
    );

    match read {
        PartitionRead::Hit {
            events,
            accounted_through,
        } => {
            assert_eq!(
                events
                    .iter()
                    .filter_map(|e| e.sequence)
                    .map(Sequence::as_i64)
                    .collect::<Vec<_>>(),
                vec![100, 101, 102, 103]
            );
            assert_eq!(accounted_through, Sequence::assigned(103));
        }
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[test]
fn the_walk_crosses_a_wholly_deleted_span_in_one_read() {
    let cache = PartitionCache::new();
    // A retention-trimmed prefix: accounted for, entirely empty.
    absorb(&cache, 100, 199, &[]);
    absorb(&cache, 200, 299, &[]);
    absorb(&cache, 300, 399, &[300]);

    // One read, not one per fetch span. A segment with no events contributes
    // accounting and nothing to deliver, so it cannot fill the limit and the
    // walk keeps going.
    match read(&cache, 99) {
        PartitionRead::Hit {
            events,
            accounted_through,
        } => {
            assert_eq!(
                events
                    .iter()
                    .filter_map(|e| e.sequence)
                    .map(Sequence::as_i64)
                    .collect::<Vec<_>>(),
                vec![300]
            );
            assert_eq!(accounted_through, Sequence::assigned(399));
            assert_eq!(events.runs(), 1, "the two empty spans contribute no run");
        }
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[test]
fn a_batchs_bytes_are_the_sum_of_its_runs() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 199, &[100, 150]);
    absorb(&cache, 200, 299, &[200]);

    let whole = match read(&cache, 99) {
        PartitionRead::Hit { events, .. } => events,
        other => panic!("expected Hit, got {other:?}"),
    };
    assert_eq!(whole.runs(), 2);
    assert_eq!(whole.len(), 3);
    assert_eq!(
        whole.bytes(),
        cache.resident_bytes(),
        "a read of everything resident must account for every resident byte"
    );
}

#[test]
fn a_refetch_after_reclamation_does_not_lower_the_published_frontier() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 199, &[100]);
    absorb(&cache, 200, 299, &[200]);
    assert_eq!(cache.newest_accounted(), Sequence::assigned(299));

    // Reclamation takes the top of the map, then a laggard refetches a span
    // below it. Recomputing the frontier from what the map now holds would
    // report 199 - and a reader at 250 would see `has_data` go false and park
    // in `wait` for an append that has already happened.
    {
        // Registered only for the pass: both spans are at or below it, so both
        // are dead. Dropped again before the refetch, so nothing reclaims the
        // refetched span out from under the assertion.
        let _reader = cache.track_reader(Sequence::assigned(299));
        cache.reclaim();
    }
    assert_eq!(cache.segment_count(), 0);
    absorb(&cache, 100, 199, &[100, 150]);

    assert_eq!(cache.newest_accounted(), Sequence::assigned(299));
}

#[test]
fn refetching_an_already_accounted_span_changes_nothing() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 199, &[100, 150]);
    let before = cache.resident_bytes();

    // Every sequence this fetch accounted for is already accounted for. It must
    // not displace the resident segment: the displaced bytes would leave
    // residency without ever being counted out of it.
    absorb(&cache, 100, 199, &[100]);

    assert_eq!(cache.segment_count(), 1);
    assert_eq!(spans(&cache), vec![(100, 199)]);
    assert_eq!(cache.resident_bytes(), before);
    assert_eq!(hit_sequences(&read(&cache, 99)), vec![100, 150]);
}

#[test]
fn a_fetch_overlapping_a_resident_span_is_narrowed_to_the_unaccounted_part() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 199, &[100, 150]);

    // Two readers at different positions can drive overlapping fetches. The
    // overlap is dropped, not recorded twice: spans must stay disjoint or a
    // walk would deliver the overlap twice over.
    absorb(&cache, 150, 249, &[150, 200]);

    assert_eq!(spans(&cache), vec![(100, 199), (200, 249)]);
    assert_eq!(
        hit_sequences(&read(&cache, 99)),
        vec![100, 150, 200],
        "150 is served once, from the segment that already accounted for it"
    );
}

#[test]
fn a_fetch_that_would_swallow_a_resident_span_stops_below_it() {
    let cache = PartitionCache::new();
    absorb(&cache, 200, 299, &[250]);

    // The incoming span runs from below the resident one to above it. Rather
    // than claim the far side, it is cut where it meets what is already known;
    // a reader that wants the far side is told `Unknown` and it is fetched
    // again.
    absorb(&cache, 100, 399, &[100, 250, 350]);

    assert_eq!(spans(&cache), vec![(100, 199), (200, 299)]);
    assert_eq!(hit_sequences(&read(&cache, 99)), vec![100, 250]);
}

#[test]
fn a_span_between_reader_clusters_is_reclaimed_and_charged_as_a_refetch() {
    let cache = PartitionCache::with_reclaim_policy(ReclaimPolicy::new(
        GapThresholdEvents(100),
        ResidencyLimitBytes(usize::MAX),
    ));
    // Densely populated spans, because a runway is a number of events: a
    // hundred-event threshold reaches across however many sequence numbers it
    // takes to find a hundred events, so segments holding one event each would
    // all sit inside one reader's runway however far apart their sequences are.
    let dense = |from: i64| (from..=from + 99).collect::<Vec<_>>();
    absorb(&cache, 100, 199, &dense(100));
    absorb(&cache, 2000, 2099, &dense(2000));
    absorb(&cache, 3000, 3099, &dense(3000));
    absorb(&cache, 5000, 5099, &dense(5000));
    // Two clusters, far enough apart in events to be separate, each having
    // consumed its own span.
    let _laggard = cache.track_reader(Sequence::assigned(199));
    let _tail = cache.track_reader(Sequence::assigned(5099));

    let report = cache.reclaim();

    // Behind every reader: nobody can ask again, so it costs nothing.
    assert_eq!(report.dead().segments(), 1);
    // The laggard's runway is spent inside 2000..=2099, so that segment stays.
    // What is beyond it, and what sits behind the tail reader, will both be
    // refetched by the laggard when it catches up - which is what bounding
    // residency this way actually costs.
    assert_eq!(report.gapped().segments(), 2);
    assert_eq!(
        cache.segment_count(),
        1,
        "the segment holding the laggard's own runway is the one that survives"
    );
    assert!(!report.limit_breached());
}

/// The property that makes a fetch shareable without anyone predicting who it
/// will serve: a span covering another reader's position removes that reader's
/// demand, wakes it, and leaves it no starvation credit for the round - all
/// keyed on what the fetch actually recorded rather than on what a threshold
/// guessed it would reach.
#[tokio::test]
async fn a_fetch_that_covers_another_reader_removes_its_demand() {
    let cache = PartitionCache::new();
    let served = cache.track_reader(Sequence::assigned(10));
    let carried = cache.track_reader(Sequence::assigned(20));

    // Both want something nothing accounts for, so both are demands.
    assert_eq!(
        cache
            .scan_demands()
            .iter()
            .map(|demand| demand.after().as_i64())
            .collect::<Vec<_>>(),
        vec![10, 20],
        "a demand names the position its reader stands on - the fetch it asks \
         for returns whatever the store holds above that, which is not a \
         sequence the broker can name in advance"
    );

    // That scan charged both readers a round of waiting, which they had: at
    // that moment nothing had been fetched for either.
    let waited_before = cache.worst_starvation();
    assert_eq!(waited_before, 1);

    // One fetch, aimed at the earlier reader, reaching past the later one.
    absorb(&cache, 11, 110, &(11..=110).collect::<Vec<_>>());

    assert!(
        cache.scan_demands().is_empty(),
        "the recorded span answers both readers, so neither still demands a fetch"
    );
    assert!(
        carried.has_data(),
        "the reader nobody fetched for was carried by the fetch that happened"
    );
    assert_eq!(
        cache.worst_starvation(),
        waited_before,
        "the scan that found both readers served charged neither of them: a \
         reader carried by another's fetch has not waited, and must not take \
         credit for the round into the next one"
    );
    let _ = served;
}

/// The other half: a fetch that stops short of a reader leaves that reader's
/// demand exactly where it was, and it is now the furthest behind.
#[tokio::test]
async fn a_fetch_that_stops_short_leaves_the_further_readers_demand() {
    let cache = PartitionCache::new();
    let _served = cache.track_reader(Sequence::assigned(10));
    let _waiting = cache.track_reader(Sequence::assigned(20));

    absorb(&cache, 11, 15, &(11..=15).collect::<Vec<_>>());

    assert_eq!(
        cache
            .scan_demands()
            .iter()
            .map(|demand| demand.after().as_i64())
            .collect::<Vec<_>>(),
        vec![20],
        "only the reader the span did not reach still needs a fetch, and it is \
         named by the position it stands on"
    );
}

#[test]
fn a_span_a_laggard_has_not_yet_read_is_kept() {
    let cache = PartitionCache::with_reclaim_policy(ReclaimPolicy::new(
        GapThresholdEvents(100),
        ResidencyLimitBytes(usize::MAX),
    ));
    absorb(&cache, 100, 199, &[100, 150]);
    let _laggard = cache.track_reader(Sequence::assigned(100));

    // The reader has consumed 100 and wants 101 onwards, which is this very
    // span. Reclaiming it would guarantee an immediate refetch.
    let report = cache.reclaim();

    assert!(!report.freed_anything());
    assert_eq!(spans(&cache), vec![(100, 199)]);
}

#[test]
fn the_byte_limit_is_held_by_absorbing_not_only_by_a_pass() {
    let cache = PartitionCache::with_reclaim_policy(ReclaimPolicy::new(
        GapThresholdEvents(100_000),
        ResidencyLimitBytes(2000),
    ));

    // Absorbing is the only thing that grows residency, so it is where a limit
    // can actually be held rather than merely audited on the next tick.
    for batch in 0..20 {
        let from = 100 + batch * 10;
        absorb(&cache, from, from + 9, &[from, from + 5]);
        assert!(
            cache.resident_bytes() <= 2000,
            "residency ran to {} on batch {batch}",
            cache.resident_bytes()
        );
    }
}

#[test]
fn the_flow_identity_holds_across_absorbing_and_reclaiming() {
    let cache = PartitionCache::new();
    let _reader = cache.track_reader(Sequence::assigned(0));

    for batch in 0..10 {
        let from = 100 + batch * 10;
        absorb(&cache, from, from + 9, &[from, from + 5]);
    }
    let _ = cache.reclaim();

    let stats = cache.stats();
    assert!(
        stats.balances(),
        "every accounted byte is either resident or reclaimed: absorbed {}, \
         reclaimed {}, resident {}",
        stats.absorbed().bytes(),
        stats.reclaimed().bytes(),
        stats.resident().bytes()
    );
    assert_eq!(stats.absorbed().segments(), 10);
    assert!(stats.peak().bytes() >= stats.resident().bytes());
}

#[test]
fn a_pass_that_frees_nothing_is_counted_separately_from_one_that_does() {
    let cache = PartitionCache::new();
    absorb(&cache, 100, 199, &[100]);
    let _reader = cache.track_reader(Sequence::assigned(199));

    let freeing = cache.reclaim();
    let empty = cache.reclaim();

    assert!(freeing.freed_anything());
    assert!(!empty.freed_anything());

    let stats = cache.stats();
    assert_eq!(stats.passes(), 2);
    assert_eq!(
        stats.freeing_passes(),
        1,
        "counting attempts would measure the ticker rather than the policy"
    );
}

/// Long enough that a wake would have arrived, short enough to keep the suite
/// fast. Used to assert a reader is *not* woken, which needs a timeout.
const BRIEF: std::time::Duration = std::time::Duration::from_millis(50);

#[tokio::test]
async fn a_reader_in_a_gap_parks_until_the_gap_is_filled() {
    let cache = PartitionCache::new();
    absorb(&cache, 300, 400, &[300]);
    let reader = cache.track_reader(Sequence::assigned(199));

    // 200 is unaccounted for even though the frontier is already at 400.
    assert!(matches!(read(&cache, 199), PartitionRead::Unknown));

    // Waiting on the frontier would resolve instantly here - it is past this
    // reader - and the reader would spin on an `Unknown` it cannot resolve,
    // permanently awake and permanently stuck. It must park instead.
    assert!(
        tokio::time::timeout(BRIEF, reader.wait()).await.is_err(),
        "a reader whose next sequence is unaccounted for must not be woken by a \
         frontier that is merely ahead of it"
    );

    absorb(&cache, 200, 299, &[250]);

    assert!(
        tokio::time::timeout(BRIEF, reader.wait()).await.is_ok(),
        "the absorb that filled the gap must wake it"
    );
    // And filling the gap reconnects the chain: 200..=299 is now adjacent to the
    // span that was already there, so one read crosses both.
    assert_eq!(hit_sequences(&read(&cache, 199)), vec![250, 300]);
}

#[tokio::test]
async fn an_absorb_wakes_only_the_readers_its_span_can_serve() {
    let cache = PartitionCache::new();
    let served = cache.track_reader(Sequence::assigned(99));
    let elsewhere = cache.track_reader(Sequence::assigned(8000));

    absorb(&cache, 100, 200, &[100]);

    assert!(
        tokio::time::timeout(BRIEF, served.wait()).await.is_ok(),
        "this span answers what the reader wants next"
    );
    assert!(
        tokio::time::timeout(BRIEF, elsewhere.wait()).await.is_err(),
        "waking a reader a fetch cannot serve is pure waste - it wakes, reads, \
         learns nothing and parks again"
    );
}

#[tokio::test]
async fn a_backfill_that_does_not_move_the_frontier_still_wakes_its_reader() {
    let cache = PartitionCache::new();
    absorb(&cache, 500, 600, &[500]);
    let laggard = cache.track_reader(Sequence::assigned(99));
    let frontier_before = cache.newest_accounted();

    absorb(&cache, 100, 200, &[100, 150]);

    assert_eq!(
        cache.newest_accounted(),
        frontier_before,
        "a span below the frontier must not move it"
    );
    assert!(
        tokio::time::timeout(BRIEF, laggard.wait()).await.is_ok(),
        "so the frontier cannot be what wakes this reader, yet it must be woken"
    );
}

#[tokio::test]
async fn an_absorb_never_wakes_a_reader_for_a_span_it_reclaims_in_the_same_call() {
    // The tightest policy expressible: keep nothing beyond what a reader reads
    // next, and hold one byte. `absorb` enforces the limit inline, so every
    // absorb here runs a full pass immediately after recording and waking.
    let cache = PartitionCache::with_reclaim_policy(ReclaimPolicy::new(
        GapThresholdEvents(0),
        ResidencyLimitBytes(1),
    ));
    let reader = cache.track_reader(Sequence::assigned(0));
    let mut expected: i64 = 1;

    for batch in 0..20 {
        let from = 1 + batch * 10;
        let present: Vec<i64> = (from..=from + 9).collect();
        absorb(&cache, from, from + 9, &present);

        assert!(
            tokio::time::timeout(BRIEF, reader.wait()).await.is_ok(),
            "the absorb covered this reader, so it must have been woken"
        );

        // And being woken has to mean something is there. If a pass could take
        // the span the same absorb just woke for, the reader would wake, find
        // nothing, and park with nobody left to wake it.
        match reader.read(ReadLimit::unbounded()) {
            PartitionRead::Hit { events, .. } => {
                for delivered in events.iter() {
                    assert_eq!(delivered.sequence, Some(Sequence::assigned(expected)));
                    expected += 1;
                }
                // No advance call: `read` moved the reader to what it accounted
                // for, which is the point of the seam.
            }
            other => panic!(
                "stalled at offset {} after absorbing {from}: {other:?}",
                reader.offset()
            ),
        }
    }

    assert_eq!(reader.offset(), Sequence::assigned(200));
    assert_eq!(
        expected, 201,
        "every event was delivered exactly once, in order"
    );
}

#[tokio::test]
async fn a_session_wakes_on_any_of_its_partitions_through_one_shared_waker() {
    // Two caches standing in for two partitions one session was assigned,
    // possibly on different topics.
    let first = PartitionCache::new();
    let second = PartitionCache::new();
    let session = std::sync::Arc::new(tokio::sync::Notify::new());
    let on_first =
        first.track_reader_sharing(Sequence::assigned(0), std::sync::Arc::clone(&session));
    let on_second =
        second.track_reader_sharing(Sequence::assigned(0), std::sync::Arc::clone(&session));

    // Only the second partition is fetched.
    absorb(&second, 1, 100, &[1, 50]);

    assert!(
        tokio::time::timeout(BRIEF, session.notified())
            .await
            .is_ok(),
        "one await must cover every partition the session holds, rather than \
         one future per partition"
    );
    // And the session then polls to find out which of them has something, which
    // it would have had to do anyway.
    assert!(matches!(
        first.read_from(on_first.offset(), ReadLimit::unbounded()),
        PartitionRead::Unknown
    ));
    assert_eq!(
        hit_sequences(&second.read_from(on_second.offset(), ReadLimit::unbounded())),
        vec![1, 50]
    );
}

#[test]
fn resident_events_after_counts_across_segments_and_does_not_subtract() {
    let cache = PartitionCache::new();
    // Deliberately sparse: 100..=200 accounts for a hundred sequences and holds
    // two events.
    absorb(&cache, 100, 200, &[100, 150]);
    absorb(&cache, 201, 300, &[250]);

    assert_eq!(
        cache.resident_events_after(Sequence::assigned(99)),
        3,
        "three events are held, however wide the spans accounting for them are"
    );
    assert_eq!(
        cache.resident_events_after(Sequence::assigned(150)),
        1,
        "only 250 remains"
    );
    assert_eq!(cache.resident_events_after(Sequence::assigned(300)), 0);
}
