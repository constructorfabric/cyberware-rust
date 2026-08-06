//! The bridge from the runway allocator to the loader's fetch sizes.
//!
//! The allocator's own convergence is tested beside it; what is at stake here
//! is that the loader inherits those properties intact - that a fetch size
//! settles, respects the shard's ceiling, ranks readers the same way, caps a
//! scanner, and reads a release as "do not fetch". Each test asserts one such
//! property rather than a particular number, so a change to the policy
//! defaults cannot quietly turn a test into a tautology.
//!
//! The whole loop runs in-process: `observe -> size -> synthetic latency ->
//! observe'`. No runtime, no cache, no storage.

use crate::domain::model::Sequence;
use std::collections::HashMap;
use std::time::Duration;

use toolkit_gts::GtsInstanceId;

use crate::domain::streaming::source::PartitionKey;
use crate::infra::partition_cache::budget::{
    EstimatedBytesPerEvent, HardLimitBytes, ShardBudget, SoftLimitBytes,
};
use crate::infra::partition_cache::runway::{EventsPerSecond, RunwayPolicy};

use super::sizing::{FetchSizer, PartitionObservation};

const BASE_LATENCY: Duration = Duration::from_millis(50);

fn key(partition: i32) -> PartitionKey {
    PartitionKey::new(
        GtsInstanceId::try_new("gts.cf.core.events.topic.v1~x.eb.orders.acme.v1")
            .expect("static topic id is valid"),
        partition,
    )
}

/// A partition sized in bytes per event as well as events per second, because
/// the budget binds on bytes and the sizer on events - a test that fixed one
/// could not exercise the other.
fn observe(
    partition: i32,
    rate: u32,
    readers: usize,
    bytes_per_event: usize,
) -> PartitionObservation {
    PartitionObservation::builder(key(partition))
        .readers(readers)
        .consumption_rate(EventsPerSecond(rate))
        .demand_from(Sequence::assigned(i64::from(partition) * 1000))
        .bytes_per_event(EstimatedBytesPerEvent::new(bytes_per_event))
        .build()
}

/// A ceiling wide enough that nothing is ever refused, for the tests that are
/// about sizing rather than about pressure.
fn unbounded_budget() -> ShardBudget {
    ShardBudget::new(SoftLimitBytes(usize::MAX), HardLimitBytes(usize::MAX))
}

/// Total bytes the sizes would commit, which is the quantity the soft limit
/// governs.
fn committed_bytes(sizes: &HashMap<PartitionKey, usize>, bytes_per_event: usize) -> usize {
    sizes
        .values()
        .map(|events| events.saturating_mul(bytes_per_event))
        .fold(0, usize::saturating_add)
}

fn size_of(sizes: &HashMap<PartitionKey, usize>, partition: i32) -> usize {
    sizes.get(&key(partition)).copied().unwrap_or(0)
}

/// Runs the loop for `ticks` rounds at a fixed latency and returns each round's
/// sizes. Fixed latency is the point: with the exogenous inputs held still, any
/// remaining movement is the sizer's own.
fn run(
    sizer: &mut FetchSizer,
    observed: &[PartitionObservation],
    ticks: usize,
) -> Vec<HashMap<PartitionKey, usize>> {
    (0..ticks)
        .map(|_| sizer.size(observed, BASE_LATENCY))
        .collect()
}

#[test]
fn a_fetch_size_settles_to_a_fixed_point_under_a_steady_rate() {
    let mut sizer = FetchSizer::new(unbounded_budget(), RunwayPolicy::default());
    let observed = vec![observe(0, 20_000, 4, 1)];

    let history = run(&mut sizer, &observed, 100);

    let settled = size_of(history.last().expect("100 ticks were run"), 0);
    let tail_moves = history
        .iter()
        .rev()
        .take(20)
        .map(|sizes| size_of(sizes, 0))
        .filter(|events| *events != settled)
        .count();

    assert_eq!(
        tail_moves, 0,
        "with the rate and the latency held fixed the fetch size must reach a \
         fixed point rather than keep moving; settled at {settled}"
    );
    // The fixed point is the bandwidth-delay product, which is the whole claim
    // the sizing rests on: 20k events per second across a 50ms refill.
    assert_eq!(settled, 1000);
}

#[test]
fn a_binding_budget_caps_the_bytes_committed_across_partitions() {
    const BYTES_PER_EVENT: usize = 10;
    const SOFT_LIMIT: usize = 14_000;

    let budget = ShardBudget::new(SoftLimitBytes(SOFT_LIMIT), HardLimitBytes(SOFT_LIMIT));
    let mut sizer = FetchSizer::new(budget, RunwayPolicy::default());
    let observed = vec![
        observe(0, 20_000, 4, BYTES_PER_EVENT),
        observe(1, 20_000, 2, BYTES_PER_EVENT),
        observe(2, 20_000, 1, BYTES_PER_EVENT),
    ];

    let history = run(&mut sizer, &observed, 100);

    for (tick, sizes) in history.iter().enumerate() {
        let committed = committed_bytes(sizes, BYTES_PER_EVENT);
        assert!(
            committed <= SOFT_LIMIT,
            "the ceiling must hold on every tick, not only at the fixed point: \
             tick {tick} committed {committed}"
        );
    }

    // Unbounded, each partition would settle at 1000 events, so the cap is
    // genuinely binding here and the assertion above is not vacuous.
    let settled = history.last().expect("100 ticks were run");
    assert!(
        committed_bytes(settled, BYTES_PER_EVENT) < 3 * 1000 * BYTES_PER_EVENT,
        "the budget was supposed to bind"
    );
    // Pressure costs throughput, never service: something must still be
    // fetchable.
    assert!(
        settled.values().any(|events| *events > 0),
        "a binding budget must degrade fetch sizes, not stop fetching entirely"
    );
}

#[test]
fn more_readers_never_earns_less_runway_at_the_same_rate() {
    const BYTES_PER_EVENT: usize = 10;
    const SOFT_LIMIT: usize = 14_000;

    let budget = ShardBudget::new(SoftLimitBytes(SOFT_LIMIT), HardLimitBytes(SOFT_LIMIT));
    let mut sizer = FetchSizer::new(budget, RunwayPolicy::default());
    // Identical in every respect but reader count, so nothing else can explain
    // a difference in the outcome.
    let observed = vec![
        observe(0, 20_000, 8, BYTES_PER_EVENT),
        observe(1, 20_000, 1, BYTES_PER_EVENT),
    ];

    let history = run(&mut sizer, &observed, 100);
    let settled = history.last().expect("100 ticks were run");

    let popular = size_of(settled, 0);
    let lonely = size_of(settled, 1);

    assert!(
        popular >= lonely,
        "residency serving more readers is worth more per byte, so it must not \
         yield first: 8 readers got {popular}, 1 reader got {lonely}"
    );
    assert!(
        committed_bytes(settled, BYTES_PER_EVENT) <= SOFT_LIMIT,
        "the comparison is only meaningful under pressure"
    );
    assert!(
        popular > lonely,
        "with the budget binding the ranking must actually separate them"
    );
}

#[test]
fn a_scanner_is_capped_rather_than_rewarded_for_its_rate() {
    let policy = RunwayPolicy::default();
    let mut consumer_sizer = FetchSizer::new(unbounded_budget(), policy.clone());
    let mut scanner_sizer = FetchSizer::new(unbounded_budget(), policy.clone());

    let consumer = vec![observe(0, 50_000, 4, 1)];
    let scanner = vec![
        PartitionObservation::builder(key(0))
            .readers(4)
            .consumption_rate(EventsPerSecond(50_000))
            .scanning(true)
            .bytes_per_event(EstimatedBytesPerEvent::new(1))
            .build(),
    ];

    let consumer_settled = size_of(
        run(&mut consumer_sizer, &consumer, 100)
            .last()
            .expect("100 ticks were run"),
        0,
    );
    let scanner_settled = size_of(
        run(&mut scanner_sizer, &scanner, 100)
            .last()
            .expect("100 ticks were run"),
        0,
    );

    assert!(
        scanner_settled <= policy.scanner_cap_events,
        "a scanner discards nearly everything it is handed, so prefetching for \
         it must be capped: got {scanner_settled}"
    );
    assert!(
        scanner_settled < consumer_settled,
        "examining fast must not outbid consuming fast: scanner \
         {scanner_settled}, consumer {consumer_settled}"
    );
}

#[test]
fn a_released_partition_is_sized_to_zero() {
    const BYTES_PER_EVENT: usize = 1000;
    let policy = RunwayPolicy::default();
    // One floored partition costs `floor_events * BYTES_PER_EVENT`, and a
    // single byte more than that admits one floor but not two - which is
    // exactly the regime where the allocator must release rather than share.
    let soft_limit = policy
        .floor_events
        .saturating_mul(BYTES_PER_EVENT)
        .saturating_add(1);

    let budget = ShardBudget::new(SoftLimitBytes(soft_limit), HardLimitBytes(soft_limit));
    let mut sizer = FetchSizer::new(budget, policy);
    let observed = vec![
        observe(0, 20_000, 8, BYTES_PER_EVENT),
        observe(1, 20_000, 1, BYTES_PER_EVENT),
    ];

    let granted = sizer.size(&observed, BASE_LATENCY);

    assert_eq!(
        size_of(&granted, 1),
        0,
        "the partition with fewest readers must be released, and a release must \
         reach the loader as zero so it fetches nothing this round"
    );
    assert!(
        size_of(&granted, 0) > 0,
        "releasing is supposed to make room for the partition worth keeping"
    );
    assert!(committed_bytes(&granted, BYTES_PER_EVENT) <= soft_limit);
}

#[test]
fn damping_state_is_dropped_when_a_partition_stops_being_observed() {
    let mut sizer = FetchSizer::new(unbounded_budget(), RunwayPolicy::default());
    let both = vec![observe(0, 20_000, 4, 1), observe(1, 20_000, 4, 1)];
    let one = vec![observe(0, 20_000, 4, 1)];

    run(&mut sizer, &both, 50);
    assert_eq!(sizer.tracked_partitions(), 2);

    let after_dropping = sizer.size(&one, BASE_LATENCY);
    assert_eq!(
        sizer.tracked_partitions(),
        1,
        "state for a retired partition must not outlive its observation, or the \
         map grows with every partition the instance has ever served"
    );

    // Behavioural proof that the state is really gone rather than merely
    // uncounted: the returning partition ramps from the floor exactly as a
    // partition the sizer has never seen does.
    let reintroduced = size_of(&sizer.size(&both, BASE_LATENCY), 1);
    let mut fresh = FetchSizer::new(unbounded_budget(), RunwayPolicy::default());
    let first_ever = size_of(&fresh.size(&one, BASE_LATENCY), 0);

    assert_eq!(
        reintroduced, first_ever,
        "a re-observed partition must restart its damping, not resume it"
    );
    assert!(
        size_of(&after_dropping, 0) > first_ever,
        "the partition that stayed observed must have kept its own history"
    );
}

/// The same observation, but with events already held ahead of the reader.
fn observe_warm(
    partition: i32,
    rate: u32,
    readers: usize,
    bytes_per_event: usize,
    resident_ahead: usize,
) -> PartitionObservation {
    PartitionObservation::builder(key(partition))
        .readers(readers)
        .consumption_rate(EventsPerSecond(rate))
        .demand_from(Sequence::assigned(i64::from(partition) * 1000))
        .bytes_per_event(EstimatedBytesPerEvent::new(bytes_per_event))
        .resident_ahead(resident_ahead)
        .build()
}

#[test]
fn a_warm_partition_is_sized_down_by_what_it_already_holds() {
    let mut cold = FetchSizer::new(unbounded_budget(), RunwayPolicy::default());
    let mut warm = FetchSizer::new(unbounded_budget(), RunwayPolicy::default());
    let latency = Duration::from_millis(50);

    let cold_sizes = cold.size(&[observe(0, 20_000, 4, 1024)], latency);
    let warm_sizes = warm.size(&[observe_warm(0, 20_000, 4, 1024, 400)], latency);

    // The grant is a residency target, so the fetch is the target minus what is
    // held. Using the target directly would re-fetch the resident 400 every
    // round, and get worse the warmer the partition became.
    assert_eq!(
        size_of(&warm_sizes, 0),
        size_of(&cold_sizes, 0).saturating_sub(400)
    );
}

#[test]
fn a_partition_already_holding_its_target_asks_for_nothing() {
    let mut sizer = FetchSizer::new(unbounded_budget(), RunwayPolicy::default());
    let latency = Duration::from_millis(50);
    let target = size_of(&sizer.size(&[observe(0, 20_000, 4, 1024)], latency), 0);

    let when_over_supplied = sizer.size(
        &[observe_warm(0, 20_000, 4, 1024, target.saturating_add(500))],
        latency,
    );

    // Saturating rather than wrapping: holding more than the target asks for
    // nothing, and must not turn into an enormous fetch.
    assert_eq!(size_of(&when_over_supplied, 0), 0);
}
