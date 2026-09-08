//! The leader-election renewal decision state machine (DESIGN.md §4.2).
//!
//! Renewal is a guarded replace setting `renewTime`, issued every
//! [`ElectionConfig::renewal_interval`](cluster_sdk::leader::ElectionConfig::renewal_interval).
//! Its *outcome* drives a small state machine that owns exactly one piece of
//! mutable state — the consecutive-missed-renewal counter — and decides whether
//! leadership continues, retries, or is lost. That decision is a pure function
//! ([`decide_renew`]) so every row of the §4.2 table is unit-testable without an
//! API server; the async loop in [`super`] only performs the I/O and applies the
//! action.
//!
//! Two authorities govern loss, and they are deliberately separate:
//!
//! - the **`Observed` deadline** (§2.8) is primary — if we cannot prove we were
//!   still inside our own lease, leadership is gone regardless of the counter;
//! - the **missed-renewal counter** is secondary — `max_missed_renewals`
//!   consecutive *retryable* failures are tolerated, and the next one loses.
//!
//! A `409` is neither: it means someone else wrote the Lease, so the claim is gone
//! *now*, and it neither counts against the budget nor is retried (treating it as
//! transient would keep a displaced leader believing it leads for the whole budget
//! — the longest split-brain window this design refuses to have).

/// The outcome of one renewal attempt, as the async loop observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewOutcome {
    /// The guarded replace applied: `renewTime` moved forward and we still hold.
    Renewed,
    /// A retryable transport/status failure (`ConnectionLost`, `Timeout`,
    /// `ResourceExhausted`) — the API server was unreachable, not a lost claim.
    Retryable,
    /// A `409 Conflict`: someone else wrote the Lease, so the claim is gone now
    /// (the re-read having confirmed the holder is no longer us, §4.2).
    Conflict,
    /// The local `Observed` deadline had already passed before the write was even
    /// attempted (§2.8) — we cannot prove we were still inside our lease.
    DeadlinePassed,
}

/// What the renewal loop does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewAction {
    /// Leadership continues. Reset the missed counter and refresh `Observed`.
    /// Emits no transition — a healthy holder renewing is not an event (§4.3).
    Continue,
    /// A tolerated transient failure. Keep the claim, retry on the next tick.
    /// Emits no transition — transient failures are handled internally (§4.2).
    Retry,
    /// Leadership is lost. Emit `Status(Lost)` and re-enter the acquire loop.
    LoseAndReenroll,
}

/// Decides the next renewal action from the `outcome`, the current consecutive
/// `missed` count, and the `max_missed` budget (§4.2). Returns the action and the
/// updated missed count.
///
/// - `Renewed` → `Continue`, counter reset to 0.
/// - `Retryable` → increment; `LoseAndReenroll` once the incremented count
///   *exceeds* `max_missed` (so exactly `max_missed` failures are tolerated),
///   otherwise `Retry`. The counter resets to 0 on loss because re-enrollment
///   starts a fresh claim.
/// - `Conflict` / `DeadlinePassed` → `LoseAndReenroll` immediately, counter reset,
///   **not** counted against the budget.
#[must_use]
pub fn decide_renew(outcome: RenewOutcome, missed: u8, max_missed: u8) -> (RenewAction, u8) {
    match outcome {
        RenewOutcome::Renewed => (RenewAction::Continue, 0),
        RenewOutcome::Retryable => {
            let missed = missed.saturating_add(1);
            if missed > max_missed {
                (RenewAction::LoseAndReenroll, 0)
            } else {
                (RenewAction::Retry, missed)
            }
        }
        // A 409 or a passed deadline is an immediate, uncounted loss.
        RenewOutcome::Conflict | RenewOutcome::DeadlinePassed => (RenewAction::LoseAndReenroll, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::{RenewAction, RenewOutcome, decide_renew};

    #[test]
    fn a_successful_renewal_continues_and_resets_the_counter() {
        // Even with prior misses on the clock, success clears them.
        assert_eq!(
            decide_renew(RenewOutcome::Renewed, 2, 2),
            (RenewAction::Continue, 0)
        );
    }

    #[test]
    fn transient_failures_are_tolerated_up_to_the_budget_then_lose() {
        // Budget 2: two failures tolerated (counter climbs, claim kept)...
        assert_eq!(
            decide_renew(RenewOutcome::Retryable, 0, 2),
            (RenewAction::Retry, 1)
        );
        assert_eq!(
            decide_renew(RenewOutcome::Retryable, 1, 2),
            (RenewAction::Retry, 2)
        );
        // ...the third exceeds the budget and loses.
        assert_eq!(
            decide_renew(RenewOutcome::Retryable, 2, 2),
            (RenewAction::LoseAndReenroll, 0)
        );
    }

    #[test]
    fn a_zero_budget_loses_on_the_first_transient_failure() {
        assert_eq!(
            decide_renew(RenewOutcome::Retryable, 0, 0),
            (RenewAction::LoseAndReenroll, 0)
        );
    }

    #[test]
    fn a_conflict_loses_immediately_without_consuming_budget() {
        // Even at missed=0 with budget to spare, a 409 is an immediate loss —
        // the claim is gone now, so retrying would be split-brain.
        assert_eq!(
            decide_renew(RenewOutcome::Conflict, 0, 5),
            (RenewAction::LoseAndReenroll, 0)
        );
    }

    #[test]
    fn a_passed_deadline_loses_immediately_without_a_write() {
        assert_eq!(
            decide_renew(RenewOutcome::DeadlinePassed, 0, 5),
            (RenewAction::LoseAndReenroll, 0)
        );
    }

    #[test]
    fn the_counter_saturates_rather_than_overflowing() {
        // At the counter ceiling the increment saturates rather than panicking; with
        // any budget below the ceiling that still means a loss.
        let (action, _missed) = decide_renew(RenewOutcome::Retryable, u8::MAX, u8::MAX - 1);
        assert_eq!(action, RenewAction::LoseAndReenroll);
        // A budget just below a mid-range count also loses on the next failure.
        assert_eq!(
            decide_renew(RenewOutcome::Retryable, 3, 3),
            (RenewAction::LoseAndReenroll, 0)
        );
    }
}
