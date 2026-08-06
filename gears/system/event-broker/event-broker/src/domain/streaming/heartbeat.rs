//! When an idle stream owes the consumer a heartbeat.
//!
//! Pure: `now` is an argument, never read. Monotonic time is not abstracted
//! behind a trait because `tokio::time` already is the injectable clock for it -
//! `pause` and `advance` intercept sleeps and `Instant::now` globally, with no
//! production indirection to pay for.

use std::time::Duration;

use tokio::time::Instant;

/// The idle-cadence timer for one stream.
#[derive(Debug, Clone, Copy)]
pub struct HeartbeatSchedule {
    interval: Duration,
    last_frame_at: Instant,
}

impl HeartbeatSchedule {
    /// Two arguments, but an interval and an instant cannot be transposed.
    #[must_use]
    pub fn new(interval: Duration, now: Instant) -> Self {
        Self {
            interval,
            last_frame_at: now,
        }
    }

    /// Records that a frame went out.
    ///
    /// **Any** frame, not only an `event` frame. A heartbeat says "this stream
    /// is alive"; a topology or progress frame has already said it, so sending a
    /// heartbeat immediately after one is noise the consumer has to parse and
    /// discard.
    pub fn record_frame(&mut self, now: Instant) {
        self.last_frame_at = now;
    }

    #[must_use]
    pub fn next_due(self) -> Instant {
        self.last_frame_at + self.interval
    }

    #[must_use]
    pub fn is_due(self, now: Instant) -> bool {
        now >= self.next_due()
    }

    #[must_use]
    pub fn interval(self) -> Duration {
        self.interval
    }
}
