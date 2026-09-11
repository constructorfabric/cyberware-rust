//! Pure: no runtime, no clock, no storage. Keep it that way.

use toolkit_gts::GtsInstanceId;

use crate::domain::model::Sequence;
use crate::domain::streaming::source::PartitionKey;

use super::budget::{
    Allocation, EstimatedBytesPerEvent, HardLimitBytes, RunwayGrant, SegmentDemand, ShardBudget,
    SoftLimitBytes,
};
use super::runway::RunwayPolicy;

fn key(partition: i32) -> PartitionKey {
    PartitionKey::new(topic_id(), partition)
}

fn topic_id() -> GtsInstanceId {
    GtsInstanceId::try_new("gts.cf.core.events.topic.v1~x.eb.orders.acme.v1")
        .expect("static topic id is valid")
}

/// One byte per event keeps the arithmetic in the assertions obvious.
fn demand(partition: i32, readers: usize, desired: usize) -> SegmentDemand {
    sized_demand(partition, readers, desired, 1)
}

fn sized_demand(
    partition: i32,
    readers: usize,
    desired: usize,
    bytes_per_event: usize,
) -> SegmentDemand {
    SegmentDemand::builder(key(partition))
        .segment_from(Sequence::assigned(i64::from(partition) * 1000))
        .readers(readers)
        .desired_runway(desired)
        .estimated_bytes_per_event(EstimatedBytesPerEvent::new(bytes_per_event))
        .build()
}

/// The allocator targets the soft limit, so these tests set both limits equal
/// and exercise that path; the hard limit belongs to the cache.
fn budget(soft: usize) -> ShardBudget {
    ShardBudget::new(SoftLimitBytes(soft), HardLimitBytes(soft))
}

fn policy(floor: usize) -> RunwayPolicy {
    RunwayPolicy {
        floor_events: floor,
        ..RunwayPolicy::default()
    }
}

fn granted(allocation: &Allocation) -> Vec<usize> {
    allocation
        .grants()
        .iter()
        .map(RunwayGrant::runway_events)
        .collect()
}

#[test]
fn no_demand_allocates_nothing() {
    let allocation = budget(1024).allocate(&[], &policy(16));

    assert!(allocation.grants().is_empty());
    assert_eq!(allocation.released_count(), 0);
}

#[test]
fn everything_fits_so_every_segment_gets_what_it_asked_for() {
    let demands = vec![demand(0, 1, 100), demand(1, 5, 200)];

    let allocation = budget(1024).allocate(&demands, &policy(16));

    assert_eq!(granted(&allocation), vec![100, 200]);
    assert_eq!(allocation.committed_bytes(&demands), 300);
    assert_eq!(allocation.released_count(), 0);
}

#[test]
fn the_allocation_never_exceeds_the_ceiling() {
    let demands = vec![demand(0, 1, 10_000), demand(1, 1, 10_000)];

    let allocation = budget(500).allocate(&demands, &policy(16));

    assert!(allocation.committed_bytes(&demands) <= 500);
}

#[test]
fn pressure_floors_everyone_before_releasing_anything() {
    // Floors total 300 of a 400-byte ceiling, so nothing is released.
    let demands = vec![demand(0, 1, 1000), demand(1, 1, 1000), demand(2, 1, 1000)];

    let allocation = budget(400).allocate(&demands, &policy(100));

    assert_eq!(allocation.released_count(), 0);
    for grant in allocation.grants() {
        assert!(
            grant.runway_events() >= 100,
            "every segment keeps its floor"
        );
    }
    assert_eq!(allocation.committed_bytes(&demands), 400);
}

#[test]
fn the_surplus_goes_to_the_segment_serving_more_readers() {
    // Floors take 200 of a 300-byte ceiling, leaving 100 of surplus.
    let demands = vec![demand(0, 1, 1000), demand(1, 10, 1000)];

    let allocation = budget(300).allocate(&demands, &policy(100));

    assert_eq!(granted(&allocation), vec![100, 200]);
}

#[test]
fn readers_per_byte_not_readers_alone_decides_the_surplus() {
    let demands = vec![sized_demand(0, 4, 1000, 1), sized_demand(1, 4, 1000, 10)];

    // Floors: 100 x 1 + 100 x 10 = 1100 bytes. Ceiling 1200 leaves 100 spare.
    let allocation = budget(1200).allocate(&demands, &policy(100));

    assert_eq!(
        granted(&allocation),
        vec![200, 100],
        "the cheaper-per-reader segment takes the surplus"
    );
}

#[test]
fn segments_are_released_only_when_floors_cannot_all_fit() {
    // Floors would need 400 bytes against a 250-byte ceiling, so two must go.
    let demands = vec![
        demand(0, 1, 1000),
        demand(1, 2, 1000),
        demand(2, 3, 1000),
        demand(3, 4, 1000),
    ];

    let allocation = budget(250).allocate(&demands, &policy(100));

    assert_eq!(
        granted(&allocation),
        vec![0, 0, 100, 100],
        "the two segments serving fewest readers are released"
    );
    assert_eq!(allocation.released_count(), 2);
    assert!(allocation.committed_bytes(&demands) <= 250);
}

#[test]
fn a_released_segment_reports_itself_released() {
    let demands = vec![demand(0, 1, 1000), demand(1, 9, 1000)];

    let allocation = budget(100).allocate(&demands, &policy(100));

    let released: Vec<bool> = allocation
        .grants()
        .iter()
        .map(RunwayGrant::is_released)
        .collect();
    assert_eq!(released, vec![true, false]);
}

#[test]
fn a_segment_desiring_less_than_the_floor_is_not_inflated_to_it() {
    // The floor is a protection, not a quota: a segment that wants 10 events
    // gets 10, and the other 90 stay available to its neighbours.
    let demands = vec![demand(0, 1, 10), demand(1, 1, 10_000)];

    let allocation = budget(200).allocate(&demands, &policy(100));

    assert_eq!(granted(&allocation).first().copied(), Some(10));
}

#[test]
fn allocation_is_deterministic_for_equal_demands() {
    let demands = vec![demand(0, 3, 1000), demand(1, 3, 1000), demand(2, 3, 1000)];
    let budget = budget(400);

    let first = budget.allocate(&demands, &policy(100));
    let second = budget.allocate(&demands, &policy(100));

    assert_eq!(first, second, "equal demands must not allocate differently");
}

#[test]
fn a_grant_is_findable_by_key_and_segment() {
    let demands = vec![demand(0, 1, 100), demand(7, 1, 250)];

    let allocation = budget(4096).allocate(&demands, &policy(16));

    assert_eq!(
        allocation.runway_for(&key(7), Sequence::assigned(7000)),
        Some(250)
    );
    assert_eq!(
        allocation.runway_for(&key(7), Sequence::assigned(999)),
        None
    );
}

#[test]
fn a_zero_byte_ceiling_releases_everything_rather_than_panicking() {
    let demands = vec![demand(0, 1, 100), demand(1, 1, 100)];

    let allocation = budget(0).allocate(&demands, &policy(16));

    assert_eq!(granted(&allocation), vec![0, 0]);
    assert_eq!(allocation.committed_bytes(&demands), 0);
}

#[test]
fn the_estimate_is_clamped_to_the_per_event_size_cap() {
    // An estimate above 64 KiB describes an event that cannot exist.
    assert_eq!(
        EstimatedBytesPerEvent::new(usize::MAX).get(),
        EstimatedBytesPerEvent::MAX
    );
    // And zero would make every segment look free.
    assert_eq!(EstimatedBytesPerEvent::new(0).get(), 1);
    assert_eq!(EstimatedBytesPerEvent::new(4096).get(), 4096);
}

#[test]
fn a_cold_partition_is_assumed_to_hold_the_largest_possible_events() {
    // Worst case until measured, so a cold segment cannot be granted runway
    // that turns out to cost far more than assumed.
    assert_eq!(
        EstimatedBytesPerEvent::cold().get(),
        EstimatedBytesPerEvent::MAX
    );
}

#[test]
fn a_hard_limit_below_the_soft_limit_is_raised_to_meet_it() {
    let shard = ShardBudget::new(SoftLimitBytes(1000), HardLimitBytes(400));

    assert_eq!(shard.soft_max_bytes(), 1000);
    assert_eq!(shard.hard_max_bytes(), 1000);
}

#[test]
fn the_minimum_band_is_one_fetch_of_largest_possible_events() {
    assert_eq!(
        ShardBudget::min_band_bytes(128),
        128 * EstimatedBytesPerEvent::MAX
    );
}

#[test]
fn a_band_narrower_than_one_fetch_is_reported_insufficient() {
    let fetch_max_events = 128;
    let needed = ShardBudget::min_band_bytes(fetch_max_events);

    let too_narrow = ShardBudget::new(SoftLimitBytes(1_000_000), HardLimitBytes(1_000_001));
    let wide_enough = ShardBudget::new(
        SoftLimitBytes(1_000_000),
        HardLimitBytes(1_000_000 + needed),
    );

    // A single absorb must not be able to cross the whole band.
    assert!(!too_narrow.has_sufficient_band(fetch_max_events));
    assert!(wide_enough.has_sufficient_band(fetch_max_events));
}
