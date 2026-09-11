//! Pure: no runtime, no clock, no storage - every duration and rate is a
//! literal. Keep it that way.

use std::time::Duration;

use super::runway::{EventsPerSecond, RunwayPolicy, RunwaySizing};

/// Undamped and unclamped, so a test can assert the bandwidth-delay product
/// itself rather than the damping applied to it.
fn raw_policy() -> RunwayPolicy {
    RunwayPolicy {
        floor_events: 0,
        scanner_cap_events: usize::MAX,
        latency_smoothing_weight: 1,
        max_step_percent: 100,
    }
}

fn sizing(latency_ms: u64, previous: usize) -> RunwaySizing {
    RunwaySizing::new(Duration::from_millis(latency_ms), previous)
}

#[test]
fn target_is_rate_times_latency() {
    let policy = raw_policy();
    let mut state = sizing(100, 0);

    let target = state.next_target(
        &policy,
        EventsPerSecond(1000),
        Duration::from_millis(100),
        false,
    );

    assert_eq!(target, 100);
}

#[test]
fn target_scales_linearly_with_latency() {
    let policy = raw_policy();
    let rate = EventsPerSecond(1000);

    let mut fast = sizing(10, 0);
    let mut slow = sizing(400, 0);

    assert_eq!(
        fast.next_target(&policy, rate, Duration::from_millis(10), false),
        10
    );
    assert_eq!(
        slow.next_target(&policy, rate, Duration::from_millis(400), false),
        400
    );
}

#[test]
fn a_slow_reader_is_sized_smaller_than_a_fast_one() {
    let policy = raw_policy();
    let latency = Duration::from_millis(200);

    let mut slow = sizing(200, 0);
    let mut fast = sizing(200, 0);

    let slow_target = slow.next_target(&policy, EventsPerSecond(10), latency, false);
    let fast_target = fast.next_target(&policy, EventsPerSecond(10_000), latency, false);

    // A slow reader needs less prefetch, not more - it consumes less while a
    // refill is in flight.
    assert_eq!(slow_target, 2);
    assert_eq!(fast_target, 2000);
    assert!(slow_target < fast_target);
}

#[test]
fn floor_applies_when_the_product_is_tiny() {
    let policy = RunwayPolicy {
        floor_events: 128,
        ..raw_policy()
    };
    let mut state = sizing(1, 0);

    // 1 event/s for 1ms rounds to 0 events; the floor keeps the segment usable.
    let target = state.next_target(&policy, EventsPerSecond(1), Duration::from_millis(1), false);

    assert_eq!(target, 128);
}

#[test]
fn a_scanner_is_capped_rather_than_sized_to_its_rate() {
    let policy = RunwayPolicy {
        floor_events: 16,
        scanner_cap_events: 256,
        ..raw_policy()
    };
    let latency = Duration::from_millis(500);
    let scanning_rate = EventsPerSecond(100_000);

    let mut consumer = sizing(500, 0);
    let mut scanner = sizing(500, 0);

    let consumer_target = consumer.next_target(&policy, scanning_rate, latency, false);
    let scanner_target = scanner.next_target(&policy, scanning_rate, latency, true);

    // Same rate, same latency - the classification is the only difference.
    assert_eq!(consumer_target, 50_000);
    assert_eq!(scanner_target, 256);
}

#[test]
fn the_scanner_cap_does_not_override_the_floor() {
    let policy = RunwayPolicy {
        floor_events: 128,
        scanner_cap_events: 256,
        ..raw_policy()
    };
    let mut state = sizing(1, 0);

    let target = state.next_target(&policy, EventsPerSecond(1), Duration::from_millis(1), true);

    assert_eq!(target, 128);
}

#[test]
fn latency_smoothing_moves_a_quarter_of_the_way_per_sample() {
    let policy = RunwayPolicy {
        latency_smoothing_weight: 4,
        ..raw_policy()
    };
    let mut state = sizing(100, 0);

    // 100ms estimate, 500ms sample, weight 4 => 100 + (500-100)/4 = 200ms.
    state.next_target(
        &policy,
        EventsPerSecond(1000),
        Duration::from_millis(500),
        false,
    );

    assert_eq!(state.smoothed_latency(), Duration::from_millis(200));
}

#[test]
fn latency_smoothing_is_symmetric_downwards() {
    let policy = RunwayPolicy {
        latency_smoothing_weight: 4,
        ..raw_policy()
    };
    let mut state = sizing(500, 0);

    // 500ms estimate, 100ms sample, weight 4 => 500 - (500-100)/4 = 400ms.
    state.next_target(
        &policy,
        EventsPerSecond(1000),
        Duration::from_millis(100),
        false,
    );

    assert_eq!(state.smoothed_latency(), Duration::from_millis(400));
}

#[test]
fn the_step_limit_bounds_movement_from_the_previous_target() {
    let policy = RunwayPolicy {
        max_step_percent: 50,
        ..raw_policy()
    };
    // Previous target 100; an abrupt jump to 10_000 may move by at most 50%.
    let mut state = sizing(1000, 100);

    let target = state.next_target(
        &policy,
        EventsPerSecond(10_000),
        Duration::from_secs(1),
        false,
    );

    assert_eq!(target, 150);
}

#[test]
fn the_step_limit_bounds_movement_downwards_too() {
    let policy = RunwayPolicy {
        max_step_percent: 50,
        ..raw_policy()
    };
    let mut state = sizing(1000, 1000);

    let target = state.next_target(&policy, EventsPerSecond(1), Duration::from_millis(1), false);

    assert_eq!(target, 500);
}

#[test]
fn repeated_recomputation_at_steady_demand_reaches_a_fixed_point() {
    let policy = RunwayPolicy::default();
    let rate = EventsPerSecond(1000);
    let latency = Duration::from_millis(200);
    let mut state = sizing(200, policy.floor_events);

    let mut target = 0;
    for _ in 0..64 {
        target = state.next_target(&policy, rate, latency, false);
    }

    assert_eq!(target, 200);
    assert_eq!(state.next_target(&policy, rate, latency, false), 200);
}

#[test]
fn an_extreme_rate_saturates_rather_than_wrapping() {
    let policy = raw_policy();
    let mut state = sizing(u64::MAX.div_euclid(1000), 0);

    let target = state.next_target(
        &policy,
        EventsPerSecond(u32::MAX),
        Duration::from_secs(u64::from(u32::MAX)),
        false,
    );

    // The multiply saturates and the division then scales it down, so the
    // result is very large rather than `usize::MAX`. What matters is that it
    // did not *wrap* to something small, which would silently starve the
    // reader.
    assert!(
        target > 1_000_000_000_000,
        "saturated target must stay huge, got {target}"
    );
}

#[test]
fn the_floor_wins_over_the_step_limit_when_ramping_up() {
    let policy = RunwayPolicy {
        floor_events: 128,
        scanner_cap_events: usize::MAX,
        latency_smoothing_weight: 1,
        max_step_percent: 50,
    };
    // Ramping up from a small previous target: the 50% step limit alone
    // would return 15, which is below the floor and therefore unusable.
    let mut state = sizing(1000, 10);
    let target = state.next_target(
        &policy,
        EventsPerSecond(10_000),
        Duration::from_secs(1),
        false,
    );
    assert!(
        target >= policy.floor_events,
        "returned {target}, below floor {}",
        policy.floor_events
    );
}
