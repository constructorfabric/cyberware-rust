//! Unit tests for the horizon door's judgement of D-04's span.
//!
//! The door's storage half is exercised end to end by
//! `tests/rest_grandfather_until.rs`, which needs a real cutover, a real approval
//! and a real store. What belongs here is the one question that needs none and
//! that no HTTP case can reach honestly: **which way this act can move the bound
//! `refuse_horizon_span_uncovered` measures**.
//!
//! The claim the module doc makes is asymmetric, and an asymmetric claim needs both
//! sides asserted or it is an assumption with a test next to it:
//!
//! * a **finite** horizon moved earlier shrinks the span, so coverage that
//!   satisfied the bound still does — and the smallest interesting shape is a key
//!   whose coverage stops *exactly* at the old floor, where a rule that forgot to
//!   recompute the floor from the proposed horizon would still pass and a rule that
//!   recomputed it wrongly would not;
//! * a horizon set on an **indefinite** generation crosses from the arm that never
//!   consults W6's margin to the arm that requires it, so `inst-gs-bound` on a plan
//!   that authors no recurring frequency is the one shape where this act can newly
//!   strand what D-04 protects.
//!
//! Each refusal is paired with the positive control that differs from it in exactly
//! the operand under test, so a guard that refused everything could not pass.

use crate::domain::error::DomainError;
use crate::domain::instant::{format_rfc3339, utc_ymd_hms};
use crate::domain::money::CurrencyCode;
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::window::{KeyWindows, WindowInterval, WindowState};
use crate::infra::window::refuse_horizon_span_uncovered;
use time::OffsetDateTime;
fn far_future_instant() -> OffsetDateTime {
    utc_ymd_hms(9999, 12, 31, 23, 59, 59)
}

/// `infra::window_tests`' scale, and for its reason: an instant no clock reaches
/// cannot make a case mean something different tomorrow.
fn at(day: i64) -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 4, 0, 0, 0) + time::Duration::days(day)
}

/// A grandfathered generation whose cohort is the cutover that created it
/// (ADR-0002), so the span's lower anchor is `at(0)` rather than the wall clock.
fn generation() -> ScopeKey {
    ScopeKey::new(
        PlanId::new(uuid::Uuid::from_u128(0x_9f_01)),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a region"),
        PhaseId::new(uuid::Uuid::from_u128(0x_fa5e)),
        PriceEligibility::ExistingGrandfathered,
        ChargeKind::Recurring,
        Cohort::Generation(at(0)),
    )
    .expect("the grandfathered class pairs with a generation cohort")
}

/// The key's coverage as `[from, to)`, `to = None` being open-ended.
fn covered(from: i64, to: Option<i64>) -> KeyWindows {
    KeyWindows {
        scope_key: generation(),
        intervals: vec![WindowInterval::new(
            at(from),
            to.map(at),
            WindowState::Scheduled,
        )],
    }
}

/// The wall clock this rule is asked at — before the cohort, so `max(cohort, now)`
/// resolves to the cohort and the cases are about the horizon rather than about
/// what day it is.
fn now() -> OffsetDateTime {
    at(-10)
}

// ---------------------------------------------------------------------------
// `inst-gs-tighten` — a finite horizon moved earlier cannot break the bound
// ---------------------------------------------------------------------------

/// The positive control: coverage that reaches the **published** floor exactly.
///
/// Horizon `at(60)`, margin 31 days, so the floor is `at(91)` and the key is
/// covered `[0, 91)`. This is the state the tightening below starts from, and it
/// has to pass or the case after it proves nothing about tightening.
#[test]
fn coverage_that_stops_at_the_floor_satisfies_the_published_horizon() {
    refuse_horizon_span_uncovered(
        &generation(),
        Some(at(60)),
        Some(time::Duration::days(31)),
        &covered(0, Some(91)),
        now(),
    )
    .expect("coverage through the floor is what the bound asks for");
}

/// `inst-gs-tighten` on that same key: the floor moves *inwards* with the horizon.
///
/// Armed against the defect the split in `infra::window` exists to prevent — a door
/// that asked the rule against the **stored** horizon. That spelling would compare
/// coverage against `at(91)` here too, so it would also pass, and this case alone
/// could not tell the two apart. The next one can.
#[test]
fn tightening_a_finite_horizon_keeps_a_key_that_already_satisfied_the_bound() {
    refuse_horizon_span_uncovered(
        &generation(),
        Some(at(30)),
        Some(time::Duration::days(31)),
        &covered(0, Some(91)),
        now(),
    )
    .expect("a floor that moved inwards is one the same coverage still reaches");
}

/// The operand check: the rule is judging the **proposed** horizon and not the
/// stored one.
///
/// Coverage `[0, 61)` does **not** reach the published floor `at(91)`, and does
/// reach the proposed one `at(60) = at(29) + 31d`. So a door that passed the stored
/// horizon in would refuse this act, and one that passes the proposed horizon
/// accepts it — the two spellings answer differently on exactly this shape. Without
/// this case the split is untested and the operand could be either.
#[test]
fn a_tightening_is_judged_against_the_horizon_it_proposes() {
    refuse_horizon_span_uncovered(
        &generation(),
        Some(at(29)),
        Some(time::Duration::days(31)),
        &covered(0, Some(61)),
        now(),
    )
    .expect("the act's own horizon is what its coverage is measured against");
}

/// And the far end is still enforced: a horizon whose floor outruns the coverage is
/// refused, which is what keeps the case above from reading as "this rule accepts
/// anything".
#[test]
fn a_horizon_whose_floor_outruns_the_coverage_is_refused() {
    let err = refuse_horizon_span_uncovered(
        &generation(),
        Some(at(80)),
        Some(time::Duration::days(31)),
        &covered(0, Some(91)),
        now(),
    )
    .expect_err("a floor at at(111) is past coverage that stops at at(91)");
    assert!(matches!(err, DomainError::WindowTrailingVoid(_)), "{err:?}");
}

// ---------------------------------------------------------------------------
// `inst-gs-bound` — the one direction that can newly break D-04
// ---------------------------------------------------------------------------

/// The positive control, and the whole reason the hazard exists: under a **null**
/// horizon the margin is never consulted at all.
///
/// A generation on a market whose plan sells recurring and authors no frequency has
/// `margin = None`, and an open-ended run from its cohort satisfies the indefinite
/// arm regardless. So this key is compliant today.
#[test]
fn an_indefinite_generation_with_no_margin_is_compliant() {
    refuse_horizon_span_uncovered(&generation(), None, None, &covered(0, None), now())
        .expect("the indefinite arm asks for an unbroken open run and nothing else");
}

/// `inst-gs-bound` on that very key is refused — the act moves it onto the arm that
/// **requires** W6's term, and the term has no value.
///
/// This is the module doc's claim, armed against the operand it names rather than
/// against "a horizon can be refused": the key, the coverage, the anchor and the
/// clock are identical to the control above and the **only** thing that changed is
/// that a bound now exists. A door that reasoned "tightening only shrinks the span,
/// so the guard cannot fire" would ship this as a silent D-04 violation.
#[test]
fn a_generation_whose_plan_has_no_margin_may_not_be_bounded() {
    let err =
        refuse_horizon_span_uncovered(&generation(), Some(at(60)), None, &covered(0, None), now())
            .expect_err("a bound whose floor has no value cannot be shown to be covered");
    let DomainError::WindowTrailingVoid(detail) = err else {
        panic!("the refusal is D-316 clause (4)'s code, got: {err:?}");
    };
    assert!(
        detail.contains("authors no frequency"),
        "the refusal must name the missing operand rather than the coverage: {detail}"
    );
}

/// The other half of `inst-gs-bound`, so the case above is not read as "setting a
/// bound is refused": with a margin, the same act on the same key passes.
#[test]
fn an_indefinite_generation_with_a_margin_may_be_bounded() {
    refuse_horizon_span_uncovered(
        &generation(),
        Some(at(60)),
        Some(time::Duration::days(31)),
        &covered(0, None),
        now(),
    )
    .expect("open-ended coverage from the cohort reaches every floor");
}

/// The leading void D-316 clause (1) added, asked of this door: a generation whose
/// coverage opens **after** its cohort is refused whatever horizon is proposed.
///
/// Here because the door is a second caller of the walk and a second caller is
/// exactly where a rule loses a clause — `refuse_horizon_span_uncovered` is one
/// body precisely so this cannot happen, and this case is what would notice if it
/// stopped being one.
#[test]
fn a_generation_whose_coverage_opens_after_its_cohort_may_not_be_bounded() {
    let err = refuse_horizon_span_uncovered(
        &generation(),
        Some(at(60)),
        Some(time::Duration::days(31)),
        // Opens thirty days after the cutover that created the generation.
        &covered(30, None),
        now(),
    )
    .expect_err("a month with no window is a month of unrateable bound periods");
    let DomainError::WindowTrailingVoid(detail) = err else {
        panic!("got: {err:?}");
    };
    assert!(
        detail.contains(&format_rfc3339(at(0))),
        "the refusal names the anchor the span runs from: {detail}"
    );
}

/// A key that is **not** a grandfathered generation is not this rule's business,
/// and the door refuses it one step earlier with a code of its own
/// (`GRANDFATHER_UNTIL_FORBIDDEN`). Asserted here because the split moved the class
/// test out of this function: a reader has to be able to see that the omission is
/// deliberate and that the walk itself is class-agnostic.
#[test]
fn the_span_walk_carries_no_class_test_of_its_own() {
    let ordinary = ScopeKey::new(
        PlanId::new(uuid::Uuid::from_u128(0x_9f_01)),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a region"),
        PhaseId::new(uuid::Uuid::from_u128(0x_fa5e)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the ordinary class pairs with cohort none");
    let err = refuse_horizon_span_uncovered(
        &ordinary,
        Some(at(60)),
        Some(time::Duration::days(31)),
        &KeyWindows {
            scope_key: ordinary.clone(),
            intervals: vec![WindowInterval::new(
                at(0),
                Some(at(10)),
                WindowState::Scheduled,
            )],
        },
        now(),
    )
    .expect_err("asked about an ordinary key, the walk answers about coverage and not about class");
    assert!(matches!(err, DomainError::WindowTrailingVoid(_)), "{err:?}");
}

// ---------------------------------------------------------------------------
// The floor is an addition, and an addition on request input can fail
// ---------------------------------------------------------------------------

/// A horizon the wire can carry that no floor can be computed from.
///
/// `grandfatherUntil` is **request input** on this door — `tighten_in` passes the
/// instant off the `PUT /bss-pricing/v1/plans/{planId}/prices/{priceId}/grandfather-until`
/// body straight in — and nothing in front of this rule bounds its range:
/// `domain::grandfather::check_tightening` returns `Ok` for **every** proposed
/// instant on an indefinite generation, since each of them is earlier than never,
/// and the field's only other check — `check_authored_instant` inside
/// `price_repo::tighten_grandfather_until` — is millisecond **precision** rather
/// than range and runs three steps later, on the write this rule guards. So
/// `DateTime::<Utc>::MAX_UTC` reaches this rule; `MAX_UTC` itself is used because
/// it is the boundary, and every millisecond-quantised instant near it overflows
/// the same add. `horizon + margin` on it was chrono's
/// `checked_add_signed(..).expect("... overflowed")` — an abort in a release
/// build, on an authenticated request path.
///
/// The coverage here is **open-ended on purpose**, and that is what makes this a
/// probe about the addition rather than about the walk: an unbroken open run
/// reaches every representable floor, so a rule that computed its floor at all
/// answers `Ok` on this shape and only the panic can make it fail. It also pins
/// the *direction* as a decision rather than as a side effect of the fixture —
/// an incomputable floor refuses here, exactly as the `None` margin above does
/// and as `domain::sellability::active_window_with_horizon` does on the same add.
/// (`domain::retirement::GenerationCoverage::span` folds the same overflow into
/// `BoundSpan::Incomputable`, which its caller reads as *keep*; that is the two
/// surfaces' standing disagreement about which way an absent operand rounds, and
/// not a third reading of it.)
#[test]
fn a_horizon_whose_floor_is_not_representable_is_refused_rather_than_panicking() {
    let err = refuse_horizon_span_uncovered(
        &generation(),
        Some(far_future_instant()),
        Some(time::Duration::days(31)),
        &covered(0, None),
        now(),
    )
    .expect_err("a floor that is not an instant is a floor no coverage can be shown to reach");
    let DomainError::WindowTrailingVoid(detail) = err else {
        panic!("the refusal is D-316 clause (4)'s code, got: {err:?}");
    };
    assert!(
        detail.contains("not a representable instant"),
        "the refusal must name the operand that could not be computed: {detail}"
    );
}
