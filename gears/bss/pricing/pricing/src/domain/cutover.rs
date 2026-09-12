//! The grandfathering cutover's compose-time judgement (`inst-gc-*`, `inst-co-*`).
//!
//! The cutover is the **second** sanctioned producer of `published → superseded`
//! (D-100), and D-127 binds it to the same unit guard as the interactive
//! supersession. Where the two units agree, this module calls the supersession's
//! spelling rather than restating it; where the design set gives them different
//! answers, it says which and why.

use toolkit_macros::domain_model;

use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::scope_key::{Cohort, ScopeKey};
use crate::domain::supersession::{ChangeoverMoment, NamedWindow, WindowShorten, changeover_floor};
use crate::domain::window::{OCCUPYING_STATES, WindowInterval, WindowState};
use time::OffsetDateTime;

/// Rule code for a cutover instant that has passed, or that no longer clears the
/// batching delay at approval commit (`07-pricewindow-linkage.md` §5, 422).
pub const CUTOVER_INSTANT_PASSED: &str = "CUTOVER_INSTANT_PASSED";

/// Is `cutover` far enough ahead of `now` for `moment`?
///
/// **One floor, two codes, and the design set is what settles it.** §5 declares
/// `SUPERSESSION_INSTANT_PASSED` as *"the same floor `inst-gc-compose` gives
/// cutovers, applied to the everyday mechanism"* — so the bound is
/// [`changeover_floor`], shared, and a second copy of it here would be the
/// hand-maintained duplicate that is how two mechanisms come to disagree about one
/// SLO. What is **not** shared is the code: an operator reading a refusal is told
/// which act they were performing, and §5 declares one code per unit.
///
/// The two floors are `inst-gc-compose`'s: strictly future at submit, and at least
/// the max batching-delay SLO ahead at approval commit. An instant inside that lag
/// would activate the successor's window while its row is not yet addressable at
/// any completed `CatalogVersion`, transiently failing renewals and arrears on the
/// key the cutover just closed.
///
/// # Errors
///
/// [`DomainError::CutoverInstantPassed`] naming the instant, the floor it missed,
/// which moment asked, and the remedy for that moment.
pub fn check_cutover_instant(
    cutover: OffsetDateTime,
    now: OffsetDateTime,
    moment: ChangeoverMoment,
) -> Result<(), DomainError> {
    changeover_floor(cutover, now, moment, "cutover").map_err(DomainError::CutoverInstantPassed)
}

/// Rule code for a cutover composed over a key with no coverage to shorten
/// (`07-pricewindow-linkage.md` §5, 422, `inst-co-shorten`).
///
/// **The supersession's identical refusal has no code**, and that asymmetry is the
/// design set's rather than this module's: §5 declares `CUTOVER_GAP` and declares
/// nothing for `inst-su-compose`'s dormant key, so that one renders as
/// `LIFECYCLE_FORBIDDEN` with the ground only in its sentence. One fact, two
/// spellings on the wire — recorded here because a reader comparing the two units
/// will otherwise read it as an inconsistency in the code.
pub const CUTOVER_GAP: &str = "CUTOVER_GAP";

/// The three window operations `inst-co-shorten`, `inst-co-copy` and
/// `inst-co-successor` compose, proven adjacent.
///
/// A struct with private construction for [`ComposedWindows`]' reason: the D-88
/// review found that two independent public instants let a caller build a shorten
/// to `T1` and a schedule from `T2 > T1`, leaving `[T1, T2)` uncovered and
/// committing cleanly, because collision is an *intersection* test and a gap is not
/// an intersection. Here there are **three** instants and the same hazard twice, so
/// they are derived from one cutover instant and cannot be set apart.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComposedCutover {
    shorten: WindowShorten,
    copy: WindowInterval,
    successor: WindowInterval,
}

impl ComposedCutover {
    /// (1) the current window, shortened to the cutover.
    #[must_use]
    pub const fn shorten(&self) -> WindowShorten {
        self.shorten
    }

    /// (2) the grandfathered copy's window, on the **new generation's** key.
    #[must_use]
    pub const fn copy(&self) -> WindowInterval {
        self.copy
    }

    /// (3) the `all_subscriptions` successor's window, on the predecessor's own key.
    #[must_use]
    pub const fn successor(&self) -> WindowInterval {
        self.successor
    }
}

/// Compose `inst-co-shorten` / `inst-co-copy` / `inst-co-successor` over one key's
/// window plane.
///
/// **The same two questions [`compose_windows`] asks, plus one the cutover adds.**
/// The first two are its verbatim: is there coverage at the instant to shorten, and
/// would the new open-ended interval collide with something already on the key. The
/// third is this unit's own — it schedules **two** windows rather than one, and the
/// copy lands on a *different* key (the new generation's), so the collision scan
/// speaks only about the `all_subscriptions` plane it was given. The copy's plane is
/// empty by construction: `inst-co-copy` mints a generation keyed by the cutover
/// instant, and a cutover at an instant an existing generation already carries is
/// refused before this function is reached.
///
/// **Why the refusals are produced here at all**: each is something a *committed*
/// cutover promises cannot happen, so compose is the only place left to answer them.
///
/// # Errors
///
/// [`DomainError::CutoverGap`] when the key is dormant at the cutover, or when the
/// covering window begins at the cutover itself — shortening it there would leave
/// the empty interval `[cutover, cutover)`, which is the same absence dormancy
/// reports reached from the other side.
/// [`DomainError::WindowOverlap`] when the key carries coverage at or after the
/// cutover that this unit would not replace.
pub fn compose_cutover_windows(
    plane: &[NamedWindow],
    cutover: OffsetDateTime,
) -> Result<ComposedCutover, DomainError> {
    let occupying = || {
        plane
            .iter()
            .filter(|window| OCCUPYING_STATES.contains(&window.interval.state))
    };

    let covering = occupying()
        .find(|window| window.interval.covers(cutover))
        .ok_or_else(|| {
            DomainError::CutoverGap(format!(
                "the canonical scope key is dormant at {}: no scheduled or active window covers \
                 that instant, and a cutover presupposes current coverage to shorten. Reviving a \
                 dormant key is a plain publish and a window schedule, never a cutover",
                format_rfc3339(cutover)
            ))
        })?;

    if covering.interval.effective_from == cutover {
        return Err(DomainError::CutoverGap(format!(
            "window {} on this canonical scope key begins at {}, the cutover itself: shortening \
             it to that instant would leave it empty, so there is no coverage for the successor \
             or the grandfathered copy to take over from. Adjust or reschedule that window \
             instead of cutting over across it",
            covering.window_id,
            format_rfc3339(cutover)
        )));
    }

    if let Some(collision) = occupying().find(|window| {
        window.window_id != covering.window_id && window.interval.effective_from >= cutover
    }) {
        return Err(DomainError::WindowOverlap(format!(
            "window {} begins at {}, which the successor's open-ended interval from {} already \
             covers; the key carries later coverage that this cutover would not replace",
            collision.window_id,
            format_rfc3339(collision.interval.effective_from),
            format_rfc3339(cutover)
        )));
    }

    Ok(ComposedCutover {
        shorten: WindowShorten {
            window_id: covering.window_id,
            effective_to: cutover,
        },
        // Both are born at the cutover, from the one instant, which is what makes the
        // handover gap-free by construction rather than by a later check.
        copy: WindowInterval::new(cutover, None, WindowState::Scheduled),
        successor: WindowInterval::new(cutover, None, WindowState::Scheduled),
    })
}

/// Mint the grandfathered copy's key: the predecessor's, moved to a **new
/// generation** (`inst-co-copy`).
///
/// **Only the copy moves.** The successor lands on the predecessor's own key —
/// that is `inst-co-supersede`, and it is why the cutover commit has to flip the
/// predecessor `published -> superseded` like a supersession does. So this function
/// changes exactly two axes, `priceEligibility` and `cohort`, and carries every
/// other one across, **including the usage line D-196 added**: a copy that dropped
/// the line would be filed under the meterless line of the same market, a key the
/// predecessor's subscribers never resolve to and one that would collide with the
/// copy of a *different* meter's cutover on the same plan.
///
/// **The duplicate generation is refused here rather than at the store.** At the
/// store it arrives as a partial-`UNIQUE` violation inside the commit — a raw driver
/// error, which is a 500 carrying nothing an operator can act on, for a request
/// whose author only has to move the instant. The same reasoning `inst-pr-return`
/// gives for keeping the save-time read beside the index.
///
/// The D-144 millisecond quantum is checked by [`ScopeKey::new`], not here: the
/// cohort axis is matched for equality against an instant another gear produced, so
/// an unquantized value builds a key nobody can find rather than a key that is
/// wrong.
///
/// # Errors
///
/// [`DomainError::DuplicateScopeKey`] when `existing_generations` already carries
/// this instant; whatever [`ScopeKey::new`] refuses otherwise (the quantum, and the
/// cohort/eligibility biconditional this function satisfies by construction).
pub fn grandfathered_copy_key(
    predecessor: &ScopeKey,
    cutover: OffsetDateTime,
    existing_generations: &[Cohort],
) -> Result<ScopeKey, DomainError> {
    if existing_generations.contains(&Cohort::Generation(cutover)) {
        return Err(DomainError::DuplicateScopeKey(format!(
            "a grandfathered generation of this canonical scope key already carries the cutover \
             instant {}; every cutover mints its own generation, so name a different instant",
            format_rfc3339(cutover)
        )));
    }
    generation_key(predecessor, cutover)
}

/// The generation key itself, **without** the duplicate check.
///
/// Split out in D-300 because a caller needs to recognise *this act's* staged copy
/// before it knows the generation set, and the check needs the set — so the two
/// cannot be one function for that caller.
///
/// **It builds nothing itself any more** (D-309). The first spelling read the
/// predecessor's axes through accessors one call at a time, which compiles
/// unchanged when the key grows and drops the new axis from every grandfathered
/// copy in silence — D-205's defect, minted in the wave that repaired its fifth
/// and sixth instances. [`ScopeKey::to_generation`] owns the construction now and
/// destructures with no rest pattern, so an eleventh axis is a compile error
/// there. This function is the name the cutover's vocabulary calls it by.
///
/// # Errors
///
/// Whatever [`ScopeKey::new`] refuses — the millisecond quantum on `cutover`
/// (D-144); the cohort/eligibility biconditional is satisfied by construction.
pub fn generation_key(
    predecessor: &ScopeKey,
    cutover: OffsetDateTime,
) -> Result<ScopeKey, DomainError> {
    predecessor.to_generation(cutover)
}

#[cfg(test)]
#[path = "cutover_tests.rs"]
mod cutover_tests;
