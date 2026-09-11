//! When a stream owes the consumer a frontier report.
//!
//! A `control:progress` frame exists for the case where a subscription's filter
//! rejects nearly everything: the consumer would otherwise see an apparently
//! idle stream and have no way to commit the ground it has actually covered.
//! Reporting the scan frontier is what lets it commit.
//!
//! Pure, and deliberately narrow: this decides *whether* a report is due and
//! says nothing about which partitions drifted. An earlier draft's
//! `select(&ReadSet, now)` pulled the whole read set - and therefore a reader
//! stub - into every scheduling test. The session composes the two instead.

use std::time::Duration;

use tokio::time::Instant;

/// How far a frontier may drift, and how often a report may be sent.
///
/// A configuration carrier: public fields and a `Default`, built from the
/// operator's config at wiring time rather than parsed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressConfig {
    /// Events a partition may examine without delivering before a report is
    /// owed.
    pub drift_threshold: usize,
    /// Floor between two reports, so a heavily filtered stream reports its
    /// frontier without flooding the consumer with frames carrying almost no
    /// new information.
    pub min_interval: Duration,
}

impl Default for ProgressConfig {
    fn default() -> Self {
        Self {
            drift_threshold: 1000,
            min_interval: Duration::from_secs(30),
        }
    }
}

/// The frontier-report timer for one stream.
#[derive(Debug, Clone, Copy)]
pub struct ProgressPolicy {
    drift_threshold: usize,
    min_interval: Duration,
    last_emitted_at: Instant,
}

impl ProgressPolicy {
    /// Two arguments, but a config reference and an instant cannot be
    /// transposed.
    #[must_use]
    pub fn new(config: &ProgressConfig, now: Instant) -> Self {
        Self {
            drift_threshold: config.drift_threshold,
            min_interval: config.min_interval,
            last_emitted_at: now,
        }
    }

    /// Whether the rate floor has elapsed. Says nothing about whether anything
    /// has actually drifted - that is the read set's to answer.
    #[must_use]
    pub fn due(self, now: Instant) -> bool {
        now >= self.next_due()
    }

    #[must_use]
    pub fn next_due(self) -> Instant {
        self.last_emitted_at + self.min_interval
    }

    pub fn record_emitted(&mut self, now: Instant) {
        self.last_emitted_at = now;
    }

    /// The drift a partition must have accumulated to be worth reporting. The
    /// session passes this to the read set; this type never holds a position.
    #[must_use]
    pub fn drift_threshold(self) -> usize {
        self.drift_threshold
    }
}
