//! Time injection point used by `HierarchyCache` for TTL evaluation.
//!
//! Production code uses `SystemClock` (wraps `std::time::Instant::now`).
//! Tests inject `StubClock` and call `advance(Duration)` to deterministically
//! push entries past their TTL without sleeping.

use std::time::Instant;
// Only the gated `StubClock` (and the unit tests) need these — keep them out
// of production builds so they don't warn as unused.
#[cfg(any(test, feature = "test-support"))]
use std::sync::Mutex;
#[cfg(any(test, feature = "test-support"))]
use std::time::Duration;
use toolkit_macros::domain_model;

/// Source of monotonic time. Implementations must be safe to share across
/// threads.
pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Production clock — reads the OS monotonic clock on every call.
#[domain_model]
#[derive(Debug, Default)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Deterministic test clock — starts at construction time and only moves
/// when callers `advance` or `set` it. Tests use this so TTL assertions
/// don't depend on real elapsed time.
#[domain_model]
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
pub struct StubClock {
    now: Mutex<Instant>,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for StubClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl StubClock {
    /// Build a clock starting at `Instant::now()`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            now: Mutex::new(Instant::now()),
        }
    }

    /// Build a clock frozen at the supplied instant.
    #[must_use]
    pub fn frozen_at(instant: Instant) -> Self {
        Self {
            now: Mutex::new(instant),
        }
    }

    /// Push the clock forward by `duration`.
    pub fn advance(&self, duration: Duration) {
        match self.now.lock() {
            Ok(mut guard) => *guard += duration,
            Err(poisoned) => *poisoned.into_inner() += duration,
        }
    }

    /// Overwrite the clock to the supplied instant.
    pub fn set(&self, instant: Instant) {
        match self.now.lock() {
            Ok(mut guard) => *guard = instant,
            Err(poisoned) => *poisoned.into_inner() = instant,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Clock for StubClock {
    fn now(&self) -> Instant {
        match self.now.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `SystemClock` is a one-line delegate to `Instant::now()`. A test that
    // slept and asserted the clock advanced was testing the standard library's
    // monotonic clock, not anything this module decides. What matters here is
    // that TTL behaviour can be driven deterministically, which is what the
    // `StubClock` tests below pin.

    #[test]
    fn stub_clock_advance_pushes_now_forward() {
        let clock = StubClock::new();
        let t0 = clock.now();
        clock.advance(Duration::from_secs(10));
        let t1 = clock.now();
        assert_eq!(t1 - t0, Duration::from_secs(10));
    }

    #[test]
    fn stub_clock_set_overwrites_now() {
        let clock = StubClock::new();
        let later = clock.now() + Duration::from_mins(1);
        clock.set(later);
        assert_eq!(clock.now(), later);
    }

    #[test]
    fn stub_clock_does_not_advance_on_its_own() {
        let clock = StubClock::new();
        let t0 = clock.now();
        std::thread::sleep(Duration::from_millis(5));
        let t1 = clock.now();
        assert_eq!(t1, t0, "StubClock must be frozen between advance() calls");
    }
}
