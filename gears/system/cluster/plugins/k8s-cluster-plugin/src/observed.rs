//! Monotonic-clock expiry (DESIGN.md §2.9).
//!
//! `renewTime` is written by the lease *holder* and read by *observers*; comparing
//! the holder's wall clock against an observer's is unsound under node clock skew
//! in both directions (a fast observer steals a live lease → split-brain; a slow
//! one refuses to take over a dead one → an outage past the TTL). Kubernetes
//! guarantees nothing about node clock synchronisation.
//!
//! Following `client-go`'s elector, a lease is considered expired **only** when the
//! observer has held an unchanged `(holderIdentity, renewTime)` record for longer
//! than `leaseDurationSeconds` on its **own** [`Instant`] clock. Every observed
//! change resets the timer; an identical re-observation does not. The record's
//! timestamp value is never compared against any clock — it is only tested for
//! equality — so skew is irrelevant on the acquisition path.

use std::time::{Duration, Instant};

/// A record last seen for a `Lease`, and when *this process* first saw it.
///
/// `R` is the observed identity — in practice the `(holderIdentity, renewTime)`
/// pair, but any `PartialEq` value works, which is what keeps this module free of
/// any Kubernetes type and exhaustively unit-testable.
#[derive(Debug, Clone)]
pub struct Observed<R> {
    record: R,
    seen_at: Instant,
}

impl<R: PartialEq> Observed<R> {
    /// Begins observing `record` as of `now`. A fresh observer must wait a full
    /// duration from here before the lease may be considered expired, even if the
    /// holder was already dead — it cannot know how long the record has been stale
    /// without trusting a foreign clock.
    pub fn new(record: R, now: Instant) -> Self {
        Self {
            record,
            seen_at: now,
        }
    }

    /// Folds a freshly read `record` in as of `now`.
    ///
    /// Resets the timer **only** when `record` differs from the one held. An
    /// identical re-observation leaves `seen_at` untouched — resetting it there
    /// would make a lease immortal (every poll would refresh a dead holder's
    /// deadline).
    pub fn observe(&mut self, record: R, now: Instant) {
        if record != self.record {
            self.record = record;
            self.seen_at = now;
        }
    }

    /// Whether the lease is expired at `now`: the unchanged record has been held
    /// for **longer than** `duration` on this observer's monotonic clock.
    #[must_use]
    pub fn is_expired(&self, now: Instant, duration: Duration) -> bool {
        now.saturating_duration_since(self.seen_at) > duration
    }
}

#[cfg(test)]
mod tests {
    use super::Observed;
    use std::time::{Duration, Instant};

    const TTL: Duration = Duration::from_secs(30);

    #[test]
    fn younger_is_live_older_is_expired() {
        let t0 = Instant::now();
        let obs = Observed::new("holder-a", t0);
        assert!(!obs.is_expired(t0 + Duration::from_secs(29), TTL));
        assert!(obs.is_expired(t0 + Duration::from_secs(31), TTL));
    }

    #[test]
    fn observed_change_resets_the_timer() {
        let t0 = Instant::now();
        let mut obs = Observed::new(("holder-a", 100u64), t0);
        // A different holder resets.
        obs.observe(("holder-b", 100), t0 + Duration::from_secs(20));
        assert!(!obs.is_expired(t0 + Duration::from_secs(40), TTL));
        // A different renewTime also resets.
        obs.observe(("holder-b", 101), t0 + Duration::from_secs(45));
        assert!(!obs.is_expired(t0 + Duration::from_secs(70), TTL));
        assert!(obs.is_expired(t0 + Duration::from_secs(80), TTL));
    }

    #[test]
    fn identical_reobservation_does_not_reset() {
        // The immortal-lease bug: re-seeing the same pair must NOT refresh seen_at.
        let t0 = Instant::now();
        let mut obs = Observed::new(("holder-a", 100u64), t0);
        obs.observe(("holder-a", 100), t0 + Duration::from_secs(20));
        obs.observe(("holder-a", 100), t0 + Duration::from_secs(29));
        assert!(obs.is_expired(t0 + Duration::from_secs(31), TTL));
    }

    #[test]
    fn fresh_observer_waits_a_full_duration() {
        // A pod that starts while the holder is already dead cannot know how long
        // the lease has been stale, so it waits one whole duration from t0.
        let t0 = Instant::now();
        let obs = Observed::new("dead-holder", t0);
        assert!(!obs.is_expired(t0 + Duration::from_secs(30), TTL));
        assert!(obs.is_expired(t0 + Duration::from_secs(30) + Duration::from_millis(1), TTL));
    }

    #[test]
    fn record_timestamp_value_is_never_read_for_expiry() {
        // The whole point: two observers with the same seen_at but wildly different
        // record timestamps (one an hour in the future, one an hour in the past)
        // behave identically, because is_expired never inspects the record value.
        let t0 = Instant::now();
        let future = Observed::new(("h", u64::MAX), t0);
        let past = Observed::new(("h", 0u64), t0);
        for elapsed in [Duration::from_secs(29), Duration::from_secs(31)] {
            assert_eq!(
                future.is_expired(t0 + elapsed, TTL),
                past.is_expired(t0 + elapsed, TTL),
            );
        }
    }
}
