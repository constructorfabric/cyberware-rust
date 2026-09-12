//! The grandfathered-row eligibility machine's authoring rules
//! (`cpt-cf-bss-pricing-state-grandfathered`, `inst-gs-bound`, `inst-gs-tighten`).
//!
//! Two transitions and one predicate between them. `inst-gs-bound` moves a
//! generation from `active_indefinite` to `active_bounded` by **setting**
//! `grandfatherUntil`; `inst-gs-tighten` keeps it in `active_bounded` by moving
//! that instant **earlier**. The third transition, `inst-gs-expire`, is
//! read-derived (`now ≥ grandfatherUntil`) and flips no stored state, so it has
//! nothing here to author.
//!
//! # One predicate for two instructions, and the design set says why
//!
//! S7 §4's entry condition (D-147) is that a row enters this machine **only**
//! where `priceEligibility = existing_grandfathered` — *"that is why
//! `inst-gs-bound`'s 'set' and `inst-gs-tighten`'s 'tighten' carry no eligibility
//! test of their own: the class is what admits the row here, and stating it twice
//! would put one rule under two owners"*. So the class test is the door's
//! (`GRANDFATHER_UNTIL_FORBIDDEN`, D-147) and what is left for the machine itself
//! is the direction of travel — which is a single comparison over the two states,
//! not two rules that happen to agree.
//!
//! [`check_tightening`] is that comparison. Setting from null and moving earlier
//! are its two accepting arms; clearing, moving later and standing still are its
//! three refusals.
//!
//! # Standing still is refused, and that is a choice worth naming
//!
//! An equal instant could have been an idempotent no-op. It is refused because
//! the act is **always material** (`inst-mat-registered`): a no-op that reached
//! the evaluator would open an approval unit over a transition that moves
//! nothing, and a second principal would be asked to authorise `T → T`. The
//! caller's remedy is to not send it, which is a remedy an operator can act on;
//! an approval queue full of null transitions is not.
//!
//! # The physical guard states the same predicate and is not this rule
//!
//! `trg_pricing_price_grandfather_monotonic` (both engines,
//! `pricing_price` as rebuilt by `pricing_price` on `SQLite` and redefined
//! by `pricing_price` on Postgres) aborts an `UPDATE` of a non-draft row that
//! nulls or raises the column. It is the floor beneath this rule, in the same
//! relationship the append-only trigger has to the lifecycle machine: it refuses
//! the write, it cannot refuse the *request*, and what reaches a caller from it is
//! a driver error with no code. Until the horizon door landed it had **no caller
//! in this crate at all** — every writer of the column wrote a draft or an insert,
//! neither of which the trigger's `OLD.lifecycle_state <> 'draft'` arm sees.

use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use time::OffsetDateTime;

/// Refuse a horizon that does not move earlier (`inst-gs-bound`,
/// `inst-gs-tighten`).
///
/// `published` is the horizon the store currently holds for the generation —
/// `None` being D-147's indefinite — and `proposed` is what the author asked for.
/// The wire cannot express a null `proposed`: clearing a bound generation would be
/// an `active_bounded → active_indefinite` edge the machine does not have, so the
/// door's DTO makes the field required rather than optional. The `Option` is
/// therefore only on the stored side.
///
/// # Errors
/// [`DomainError::GrandfatherLoosenForbidden`] naming both instants, so the author
/// corrects one value rather than resubmitting and guessing which end was wrong.
pub fn check_tightening(
    published: Option<OffsetDateTime>,
    proposed: OffsetDateTime,
) -> Result<(), DomainError> {
    let Some(published) = published else {
        // `inst-gs-bound`: setting a horizon on an indefinite generation is a
        // tightening of infinity, and every finite instant is earlier than that.
        // The same reading `materiality::triggers::tightens_horizon` takes, and
        // deliberately the same: one direction, decided once.
        return Ok(());
    };
    if proposed < published {
        return Ok(());
    }
    Err(DomainError::GrandfatherLoosenForbidden(format!(
        "this generation is grandfathered until {}, and {} is not earlier than that; the horizon \
         is the instant retained subscribers stop being eligible, so it only ever moves earlier - \
         a later one re-grants an eligibility a second principal already approved the end of, and \
         an equal one is a transition that moves nothing",
        format_rfc3339(published),
        format_rfc3339(proposed)
    )))
}

#[cfg(test)]
#[path = "grandfather_tests.rs"]
mod tests;
