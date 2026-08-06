//! Backoff for a tail that has not materialised yet.
//!
//! A cluster notification can arrive before the backend has assigned the event
//! a sequence and stored it, so a fetch aimed at the tail can legitimately come
//! back empty. Treating that as "the partition is idle" strands every reader
//! parked there: the notification for that event has already fired, so nothing
//! will wake them. This is what keeps asking.
//!
//! Pure - `now` is passed in rather than read, so a test can drive it exactly.

use std::time::Duration;

use tokio::time::Instant;

/// How hard to keep asking a tail that keeps coming back empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollPolicy {
    floor: Duration,
    ceiling: Duration,
}

impl PollPolicy {
    /// Two arguments of the same type would be transposable, so this takes the
    /// floor and widens to a ceiling.
    #[must_use]
    pub fn from_floor(floor: Duration) -> Self {
        Self {
            floor,
            ceiling: floor.saturating_mul(64),
        }
    }

    #[must_use]
    pub fn up_to(mut self, ceiling: Duration) -> Self {
        self.ceiling = ceiling.max(self.floor);
        self
    }

    #[must_use]
    pub fn floor(self) -> Duration {
        self.floor
    }

    #[must_use]
    pub fn ceiling(self) -> Duration {
        self.ceiling
    }
}

impl Default for PollPolicy {
    /// A millisecond, doubling to roughly a tenth of a second. Fast enough that
    /// a sequence appearing is noticed promptly, slow enough that a genuinely
    /// idle partition costs almost nothing.
    fn default() -> Self {
        Self::from_floor(Duration::from_millis(1)).up_to(Duration::from_millis(64))
    }
}

/// One partition's tail-poll state.
#[derive(Debug, Clone, Copy)]
pub struct TailPoll {
    backoff: Duration,
    ready_at: Option<Instant>,
}

impl TailPoll {
    #[must_use]
    pub fn new(policy: PollPolicy) -> Self {
        Self {
            backoff: policy.floor(),
            ready_at: None,
        }
    }

    /// Whether a tail fetch may be issued now.
    ///
    /// Only tail fetches ask this. A backfill covers sequences the backend
    /// certainly holds, so it is always eligible - gating it behind a tail that
    /// does not exist yet would stall a lagging reader for something unrelated
    /// to it.
    #[must_use]
    pub fn is_ready(self, now: Instant) -> bool {
        self.ready_at.is_none_or(|ready| now >= ready)
    }

    /// A fetch returned events, so the tail is real and moving: ask again
    /// immediately next time.
    pub fn found_events(&mut self, policy: PollPolicy) {
        self.backoff = policy.floor();
        self.ready_at = None;
    }

    /// A fetch came back empty. Wait longer before asking again, up to the
    /// ceiling.
    pub fn found_nothing(&mut self, now: Instant, policy: PollPolicy) {
        self.ready_at = Some(now + self.backoff);
        self.backoff = self
            .backoff
            .saturating_mul(2)
            .min(policy.ceiling())
            .max(policy.floor());
    }

    #[must_use]
    pub fn backoff(self) -> Duration {
        self.backoff
    }
}
