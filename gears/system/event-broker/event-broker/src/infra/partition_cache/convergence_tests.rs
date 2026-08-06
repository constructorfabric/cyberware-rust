//! Convergence of the runway control loop.
//!
//! Sizing on an exogenous consumption rate removes the first-order feedback
//! loop by construction, but a second-order one remains: refill latency
//! depends on loader queueing, and queueing depends on total demand. Smoothing
//! and step-limiting are supposed to damp it. That is the weakest claim in the
//! design and the cheapest to check, and these tests are the check.
//!
//! The whole loop is simulated in-process: `demand -> allocate -> synthetic
//! latency -> demand'`. No runtime, no cache, no storage.

use std::time::Duration;

use toolkit_gts::GtsInstanceId;

use crate::domain::model::Sequence;
use crate::domain::streaming::source::PartitionKey;

use super::budget::{
    EstimatedBytesPerEvent, HardLimitBytes, SegmentDemand, ShardBudget, SoftLimitBytes,
};
use super::runway::{EventsPerSecond, RunwayPolicy, RunwaySizing};

/// One simulated segment: a fixed, exogenous consumption rate plus its own
/// damping state.
struct Segment {
    key: PartitionKey,
    rate: EventsPerSecond,
    readers: usize,
    scanning: bool,
    sizing: RunwaySizing,
}

fn segment(partition: i32, rate: u32, readers: usize) -> Segment {
    Segment {
        key: PartitionKey::new(
            GtsInstanceId::try_new("gts.cf.core.events.topic.v1~x.eb.orders.acme.v1")
                .expect("static topic id is valid"),
            partition,
        ),
        rate: EventsPerSecond(rate),
        readers,
        scanning: false,
        sizing: RunwaySizing::new(BASE_LATENCY, 128),
    }
}

const BASE_LATENCY: Duration = Duration::from_millis(50);

/// Refill latency as a function of how much the shard is holding: a base cost
/// plus a term that grows with committed bytes, which is what makes the
/// second-order loop exist at all. Deterministic, so the simulation is
/// reproducible.
fn synthetic_latency(committed_bytes: usize) -> Duration {
    let queue_micros = u64::try_from(committed_bytes)
        .unwrap_or(u64::MAX)
        .saturating_mul(20);
    BASE_LATENCY.saturating_add(Duration::from_micros(queue_micros))
}

/// Committed bytes after each tick. Every simulated segment is one byte per
/// event, so this doubles as the total granted runway.
fn run(
    segments: &mut [Segment],
    budget: &ShardBudget,
    policy: &RunwayPolicy,
    ticks: usize,
) -> Vec<usize> {
    let mut history = Vec::with_capacity(ticks);
    let mut latency = BASE_LATENCY;

    for _ in 0..ticks {
        let demand: Vec<SegmentDemand> = segments
            .iter_mut()
            .map(|slot| {
                let desired = slot
                    .sizing
                    .next_target(policy, slot.rate, latency, slot.scanning);
                SegmentDemand::builder(slot.key.clone())
                    .segment_from(Sequence::assigned(i64::from(slot.key.partition) * 1000))
                    .readers(slot.readers)
                    .desired_runway(desired)
                    .estimated_bytes_per_event(EstimatedBytesPerEvent::new(1))
                    .build()
            })
            .collect();

        let allocation = budget.allocate(&demand, policy);
        let committed = allocation.committed_bytes(&demand);

        latency = synthetic_latency(committed);
        history.push(committed);
    }

    history
}

/// Largest tick-to-tick change over the last `window` samples - the measure of
/// how much the loop is still moving once it has had time to settle.
fn tail_swing(history: &[usize], window: usize) -> usize {
    let tail: Vec<usize> = history.iter().rev().take(window).copied().collect();
    tail.windows(2)
        .map(|pair| {
            pair.first()
                .unwrap_or(&0)
                .abs_diff(*pair.get(1).unwrap_or(&0))
        })
        .max()
        .unwrap_or(0)
}

#[test]
fn the_loop_settles_under_steady_exogenous_demand() {
    let mut segments = vec![segment(0, 2000, 4), segment(1, 500, 2)];
    let budget = ShardBudget::new(SoftLimitBytes(8192), HardLimitBytes(8192));
    let policy = RunwayPolicy::default();

    let history = run(&mut segments, &budget, &policy, 200);

    assert_eq!(
        tail_swing(&history, 20),
        0,
        "with fixed rates the allocation must reach a fixed point, not keep moving"
    );
}

#[test]
fn the_loop_settles_when_the_budget_binds() {
    // Desires far exceed the ceiling, so every tick runs the pressure path.
    let mut segments = vec![segment(0, 50_000, 8), segment(1, 50_000, 1)];
    let budget = ShardBudget::new(SoftLimitBytes(512), HardLimitBytes(512));
    let policy = RunwayPolicy::default();

    let history = run(&mut segments, &budget, &policy, 200);

    assert!(
        history.iter().all(|committed| *committed <= 512),
        "the ceiling must hold on every tick, not just at the fixed point"
    );
    assert_eq!(tail_swing(&history, 20), 0);
}

#[test]
fn damping_reduces_the_swing_it_is_there_to_reduce() {
    let damped = RunwayPolicy {
        latency_smoothing_weight: 8,
        max_step_percent: 25,
        ..RunwayPolicy::default()
    };
    let undamped = RunwayPolicy {
        latency_smoothing_weight: 1,
        max_step_percent: 100,
        ..RunwayPolicy::default()
    };

    let mut damped_segments = vec![segment(0, 20_000, 4), segment(1, 8000, 2)];
    let mut undamped_segments = vec![segment(0, 20_000, 4), segment(1, 8000, 2)];
    let budget = ShardBudget::new(SoftLimitBytes(4096), HardLimitBytes(4096));

    let damped_history = run(&mut damped_segments, &budget, &damped, 120);
    let undamped_history = run(&mut undamped_segments, &budget, &undamped, 120);

    let damped_swing = peak_swing(&damped_history);
    let undamped_swing = peak_swing(&undamped_history);

    assert!(
        damped_swing <= undamped_swing,
        "damping must not make the loop worse: damped swing {damped_swing}, \
         undamped swing {undamped_swing}"
    );
}

/// Largest tick-to-tick change anywhere in the run, including the transient.
fn peak_swing(history: &[usize]) -> usize {
    history
        .windows(2)
        .map(|pair| {
            pair.first()
                .unwrap_or(&0)
                .abs_diff(*pair.get(1).unwrap_or(&0))
        })
        .max()
        .unwrap_or(0)
}

#[test]
fn a_rate_step_is_absorbed_without_running_away() {
    let policy = RunwayPolicy::default();
    let budget = ShardBudget::new(SoftLimitBytes(8192), HardLimitBytes(8192));
    let mut segments = vec![segment(0, 1000, 4)];

    let before = run(&mut segments, &budget, &policy, 80);
    assert_eq!(tail_swing(&before, 10), 0, "settled before the step");

    if let Some(slot) = segments.first_mut() {
        slot.rate = EventsPerSecond(10_000);
    }
    let after = run(&mut segments, &budget, &policy, 200);

    assert_eq!(
        tail_swing(&after, 20),
        0,
        "the loop must re-settle after a step change rather than oscillate"
    );
    assert!(
        after.last().copied().unwrap_or(0) > before.last().copied().unwrap_or(0),
        "a faster reader should end up holding more runway"
    );
}

#[test]
fn a_scanner_does_not_drive_the_loop_despite_a_high_rate() {
    let policy = RunwayPolicy::default();
    let budget = ShardBudget::new(SoftLimitBytes(8192), HardLimitBytes(8192));

    let mut consumers = vec![segment(0, 50_000, 4)];
    let mut scanners = vec![segment(0, 50_000, 4)];
    if let Some(slot) = scanners.first_mut() {
        slot.scanning = true;
    }

    let consumer_history = run(&mut consumers, &budget, &policy, 150);
    let scanner_history = run(&mut scanners, &budget, &policy, 150);

    let consumer_settled = consumer_history.last().copied().unwrap_or(0);
    let scanner_settled = scanner_history.last().copied().unwrap_or(0);

    assert!(
        scanner_settled < consumer_settled,
        "a scanner examines fast but must be capped, not rewarded: scanner \
         {scanner_settled}, consumer {consumer_settled}"
    );
    assert_eq!(tail_swing(&scanner_history, 20), 0);
}
