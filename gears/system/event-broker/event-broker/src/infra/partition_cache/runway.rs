//! Runway sizing: how many events a segment keeps resident ahead of its
//! slowest reader.
//!
//! Runway is a bandwidth-delay product - `consumption_rate x refill_latency` -
//! because that is what a reader consumes while one refill is in flight. The
//! rate must be the reader's own, set by its client and therefore exogenous:
//! sizing from observed *shortfall* instead closes a positive feedback loop
//! through the reader's own throughput, and diverges in both directions.
//!
//! A second-order loop remains, because refill latency depends on how much the
//! shard is holding. It is damped rather than removed: the latency estimate is
//! smoothed, and one recomputation's movement is bounded.
//!
//! Arithmetic is integer throughout. The values are counts of events and
//! durations, so floats would buy nothing, and the crate denies
//! `cast_precision_loss` and `float_cmp` - fixed-point keeps both the code and
//! its tests exact.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EventsPerSecond(pub u32);

/// Operator-facing sizing configuration. Stateless: the damping state is
/// per-segment and mutable, so it lives in [`RunwaySizing`] instead - resist
/// adding a mutable field here.
#[derive(Debug, Clone)]
pub struct RunwayPolicy {
    /// Never size a segment below this. A floored segment is still served
    /// correctly, it just refills more often - which is what makes budget
    /// pressure cost throughput rather than lost events.
    pub floor_events: usize,
    /// Ceiling for a segment whose readers are classified as scanning.
    /// Prefetching for a reader that discards nearly everything benefits no
    /// reader, so a scanner is capped rather than sized to its high
    /// examination rate.
    pub scanner_cap_events: usize,
    /// Exponential-moving-average divisor for the latency estimate: a new
    /// sample contributes `1/latency_smoothing_weight`. `1` disables
    /// smoothing.
    pub latency_smoothing_weight: u32,
    /// Largest move, as a percentage of the previous target, that one
    /// recomputation may make. `100` disables step limiting.
    pub max_step_percent: u32,
}

impl Default for RunwayPolicy {
    fn default() -> Self {
        Self {
            floor_events: 128,
            scanner_cap_events: 256,
            latency_smoothing_weight: 4,
            max_step_percent: 50,
        }
    }
}

/// Per-segment damping state, retained across recomputations.
#[derive(Debug, Clone)]
pub struct RunwaySizing {
    smoothed_latency_micros: u64,
    previous_target: usize,
}

impl RunwaySizing {
    /// Seeding with a real first observation keeps the first `next_target`
    /// from stepping out of zero.
    #[must_use]
    pub fn new(initial_latency: Duration, initial_target: usize) -> Self {
        Self {
            smoothed_latency_micros: micros(initial_latency),
            previous_target: initial_target,
        }
    }

    /// Exposed for observability and for the convergence tests.
    #[must_use]
    pub fn smoothed_latency(&self) -> Duration {
        Duration::from_micros(self.smoothed_latency_micros)
    }

    #[must_use]
    pub fn previous_target(&self) -> usize {
        self.previous_target
    }

    /// Bounds are applied after the step limit, so `floor_events` wins over
    /// `max_step_percent`. Ordered the other way, a target ramping up from a
    /// small previous value is held below the floor for several
    /// recomputations - the floor is a correctness bound, the step limit only
    /// a preference.
    pub fn next_target(
        &mut self,
        policy: &RunwayPolicy,
        rate: EventsPerSecond,
        observed_latency: Duration,
        scanning: bool,
    ) -> usize {
        self.smooth_latency(policy, observed_latency);

        let bandwidth_delay = bandwidth_delay_product(rate, self.smoothed_latency_micros);
        let stepped = self.limit_step(policy, bandwidth_delay);
        let target = clamp_to_bounds(stepped, policy, scanning);

        self.previous_target = target;
        target
    }

    /// `smoothed += (sample - smoothed) / weight`, branched so the integer
    /// arithmetic cannot underflow when the sample is below the estimate.
    fn smooth_latency(&mut self, policy: &RunwayPolicy, observed: Duration) {
        let weight = u64::from(policy.latency_smoothing_weight.max(1));
        let sample = micros(observed);
        let current = self.smoothed_latency_micros;

        self.smoothed_latency_micros = if sample >= current {
            current.saturating_add(sample.saturating_sub(current).div_euclid(weight))
        } else {
            current.saturating_sub(current.saturating_sub(sample).div_euclid(weight))
        };
    }

    /// Damping for the second-order loop: unclamped, one latency spike swings
    /// the allocation to an extreme and the correction overshoots.
    fn limit_step(&self, policy: &RunwayPolicy, target: usize) -> usize {
        let percent = u64::from(policy.max_step_percent);
        if percent >= 100 || self.previous_target == 0 {
            return target;
        }

        let previous = as_u64(self.previous_target);
        let span = previous.saturating_mul(percent).div_euclid(100).max(1);
        let lower = previous.saturating_sub(span);
        let upper = previous.saturating_add(span);

        as_usize(as_u64(target).clamp(lower, upper))
    }
}

/// Saturating rather than wrapping: an extreme rate or latency yields a very
/// large target that the budget then refuses, never a wrapped small one that
/// would silently starve a reader.
fn bandwidth_delay_product(rate: EventsPerSecond, latency_micros: u64) -> usize {
    const MICROS_PER_SECOND: u64 = 1_000_000;

    let events = u64::from(rate.0)
        .saturating_mul(latency_micros)
        .div_euclid(MICROS_PER_SECOND);
    as_usize(events)
}

fn clamp_to_bounds(target: usize, policy: &RunwayPolicy, scanning: bool) -> usize {
    let ceiling = if scanning {
        policy.scanner_cap_events
    } else {
        usize::MAX
    };
    target.max(policy.floor_events).min(ceiling)
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn as_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}
