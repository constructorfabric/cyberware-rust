//! Pure: spans, reader positions and counts. No cache, no runtime, no backend.
//!
//! The claim under test is the one the whole architecture rests on - that the
//! number of fetches tracks reader *clusters* and not readers - so most of these
//! are about how many demands come out, not what is in them.

use crate::domain::model::Sequence;

use super::demand::{Demand, FetchReason, ReaderNeed, StarvationWeight, derive, rank, unserved};
use super::reclaim::SegmentSummary;

fn resident(from: i64, through: i64) -> SegmentSummary {
    let from = Sequence::assigned(from);
    let through = Sequence::assigned(through);
    SegmentSummary::builder(from)
        .through(through)
        .events(1)
        .bytes(1)
        .build()
}

/// A reader standing on `position` - the sequence it has consumed, not the one
/// it wants next, which is unknowable in a sparse space.
fn need(position: i64) -> ReaderNeed {
    ReaderNeed::new(Sequence::assigned(position))
}

#[test]
fn a_reader_a_resident_span_can_answer_makes_no_demand() {
    let summaries = [resident(100, 200)];

    // Inside the span, so it is answerable from memory - with an event or with
    // the knowledge that one was deleted.
    assert!(unserved(&summaries, &[need(150)]).is_empty());
    assert!(derive(&summaries, &[need(150)], Sequence::assigned(200)).is_empty());
}

#[test]
fn a_thousand_readers_at_one_position_make_one_demand() {
    let needs: Vec<ReaderNeed> = (0..1000).map(|_| need(500)).collect();

    let demands = derive(&[resident(100, 200)], &needs, Sequence::assigned(200));

    // The load-bearing claim: fetches track clusters, not readers.
    assert_eq!(demands.len(), 1);
    assert_eq!(
        demands.first().map(|demand| demand.readers_behind()),
        Some(1000)
    );
}

/// Readers spread across distinct positions are distinct needs: each demand is
/// aimed at a reader that genuinely stands there. Nothing here predicts that
/// the first fetch will also serve the others - it usually will, and
/// `a_fetch_that_covers_another_reader_removes_its_demand` is where that is
/// asserted, against what a fetch actually recorded.
#[test]
fn readers_at_distinct_positions_are_distinct_demands_furthest_behind_first() {
    let needs: Vec<ReaderNeed> = (0..64).map(|index| need(500 + index)).collect();

    let demands = derive(&[resident(100, 200)], &needs, Sequence::assigned(200));

    assert_eq!(demands.len(), 64);
    assert_eq!(
        demands.first().map(|demand| demand.after()),
        Some(Sequence::assigned(500)),
        "the furthest behind comes first, because a fetch from there can carry \
         the readers ahead of it and one from any of them never carries it"
    );
    assert!(
        demands.iter().all(|demand| demand.readers_behind() == 1),
        "one reader stands at each position, and the count says so rather than \
         crediting a demand with readers a fetch may not reach"
    );
}

#[test]
fn readers_far_apart_make_separate_demands() {
    let demands = derive(
        &[resident(100, 200)],
        &[need(500), need(9000)],
        Sequence::assigned(200),
    );

    assert_eq!(demands.len(), 2);
    assert_eq!(
        demands
            .iter()
            .map(|demand| demand.after().as_i64())
            .collect::<Vec<_>>(),
        vec![500, 9000]
    );
}

/// Chaining is gone with the estimate that produced it. Three readers at three
/// positions are three demands, ordered so the one that can carry the others
/// goes first; whether it does is decided by the span its fetch records.
#[test]
fn readers_at_three_positions_are_three_demands_in_position_order() {
    let demands = derive(
        &[resident(100, 200)],
        &[need(680), need(500), need(590)],
        Sequence::assigned(200),
    );

    assert_eq!(
        demands
            .iter()
            .map(|demand| demand.after().as_i64())
            .collect::<Vec<_>>(),
        vec![500, 590, 680],
        "needs arrive in any order and come out furthest-behind first"
    );
}

#[test]
fn an_empty_partition_asks_for_a_cold_start() {
    let demands = derive(&[], &[need(1)], Sequence::assigned(0));

    assert_eq!(
        demands.first().map(|demand| demand.reason()),
        Some(FetchReason::ColdStart)
    );
}

#[test]
fn a_demand_above_the_frontier_is_a_tail_and_defers_to_backoff() {
    let demands = derive(&[resident(100, 200)], &[need(201)], Sequence::assigned(200));

    let demand = demands.first().copied().expect("one demand");
    assert_eq!(demand.reason(), FetchReason::Tail);
    assert!(
        demand.defers_to_backoff(),
        "the events may not exist yet, so hammering the backend is wrong"
    );
}

/// The boundary case, and the one that livelocks the loader when it is wrong.
///
/// A reader standing *at* the frontier has consumed everything anything has
/// accounted for, so what it asks for next is above the frontier and may not
/// exist yet. Calling that a backfill claims the sequences certainly exist,
/// which exempts the demand from the poller's backoff and lets it win its
/// partition's single fetch slot every round: the fetch returns nothing, the
/// readers genuinely behind it are never served, and starvation grows without
/// bound. `FetchReason::Tail`'s own definition is "at or beyond", and this is
/// the "at".
#[test]
fn a_demand_exactly_at_the_frontier_is_a_tail_not_a_backfill() {
    let demands = derive(&[resident(100, 200)], &[need(200)], Sequence::assigned(200));

    let demand = demands.first().copied().expect("one demand");
    assert_eq!(demand.reason(), FetchReason::Tail);
    assert!(
        demand.defers_to_backoff(),
        "a caught-up reader's fetch is speculative and belongs behind the \
         poller's backoff, or it starves every reader behind it"
    );
}

#[test]
fn a_demand_below_the_frontier_is_a_backfill_and_never_waits_on_backoff() {
    // 250 was reclaimed or never fetched, but something accounted as far as 400,
    // so these sequences exist and are worth asking for immediately.
    let demands = derive(
        &[resident(100, 200), resident(300, 400)],
        &[need(250)],
        Sequence::assigned(400),
    );

    let demand = demands.first().copied().expect("one demand");
    assert_eq!(demand.reason(), FetchReason::Backfill);
    assert!(
        !demand.defers_to_backoff(),
        "a tail that has not materialised must not gate a laggard"
    );
}

/// Readers on one position share one fetch, so the demand carries the worst
/// wait among them - otherwise a laggard standing behind a freshly arrived
/// reader would have its credit averaged away.
#[test]
fn a_demand_inherits_the_worst_starvation_at_its_position() {
    let needs = [
        need(500),
        need(500).starved_for(7),
        need(500).starved_for(3),
    ];

    let demands = derive(&[resident(100, 200)], &needs, Sequence::assigned(200));

    assert_eq!(demands.len(), 1);
    assert_eq!(
        demands.first().map(|demand| demand.starved_rounds()),
        Some(7)
    );
}

#[test]
fn fan_out_leads_while_nothing_has_waited_long() {
    let mut demands = vec![
        Demand::builder(Sequence::assigned(9000))
            .readers_behind(1)
            .starved_rounds(2)
            .build(),
        Demand::builder(Sequence::assigned(500))
            .readers_behind(1000)
            .starved_rounds(0)
            .build(),
    ];

    rank(&mut demands, StarvationWeight::default());

    // Coalescing exists to exploit exactly this: one fetch serving a thousand
    // readers is worth a thousand times one serving a single reader.
    assert_eq!(
        demands.first().map(|demand| demand.after()),
        Some(Sequence::assigned(500))
    );
}

#[test]
fn starvation_credit_eventually_overturns_fan_out() {
    let weight = StarvationWeight::default();
    let popular = Demand::builder(Sequence::assigned(500))
        .readers_behind(1000)
        .starved_rounds(0)
        .build();

    // Ten reader-equivalents a scan, so a single lagging reader needs a hundred
    // scans to outweigh a thousand at the tail. That bound is the fairness
    // guarantee - not an override of fan-out, a limit on how long it can win.
    let waited_a_while = Demand::builder(Sequence::assigned(9000))
        .readers_behind(1)
        .starved_rounds(99)
        .build();
    let waited_too_long = Demand::builder(Sequence::assigned(9000))
        .readers_behind(1)
        .starved_rounds(101)
        .build();

    assert!(waited_a_while.value(weight) < popular.value(weight));
    assert!(waited_too_long.value(weight) > popular.value(weight));
}

#[test]
fn a_zero_weight_is_pure_fan_out_and_can_starve_a_laggard_forever() {
    let weight = StarvationWeight(0);
    let popular = Demand::builder(Sequence::assigned(500))
        .readers_behind(1000)
        .build();
    let ancient = Demand::builder(Sequence::assigned(9000))
        .readers_behind(1)
        .starved_rounds(u32::MAX)
        .build();

    assert!(
        ancient.value(weight) < popular.value(weight),
        "documented consequence of turning fairness off, not an accident"
    );
}

#[test]
fn value_is_measured_in_reader_equivalents() {
    let demand = Demand::builder(Sequence::assigned(500))
        .readers_behind(40)
        .starved_rounds(6)
        .build();

    assert_eq!(
        demand.value(StarvationWeight(10)),
        100,
        "40 readers plus 6 x 10"
    );
}

#[test]
fn ranking_is_total_so_a_round_is_reproducible() {
    let mut demands = vec![
        Demand::builder(Sequence::assigned(900))
            .readers_behind(5)
            .starved_rounds(0)
            .build(),
        Demand::builder(Sequence::assigned(100))
            .readers_behind(5)
            .starved_rounds(0)
            .build(),
        Demand::builder(Sequence::assigned(500))
            .readers_behind(5)
            .starved_rounds(0)
            .build(),
    ];

    rank(&mut demands, StarvationWeight::default());

    assert_eq!(
        demands
            .iter()
            .map(|demand| demand.after().as_i64())
            .collect::<Vec<_>>(),
        vec![100, 500, 900],
        "identical demands must order by position rather than by arrival"
    );
}
