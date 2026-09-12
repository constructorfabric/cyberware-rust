//! Tests for the grandfathering cutover's compose-time refusals.

use uuid::Uuid;

use super::{
    CUTOVER_INSTANT_PASSED, check_cutover_instant, compose_cutover_windows, grandfathered_copy_key,
};
use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::money::CurrencyCode;
use crate::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::supersession::{
    ChangeoverMoment, MAX_BATCHING_DELAY, NamedWindow, SUPERSESSION_INSTANT_PASSED,
    changeover_floor, check_changeover_instant,
};
use crate::domain::window::{WindowInterval, WindowState};
use time::OffsetDateTime;

fn now() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 6, 12, 0, 0)
}

fn message(err: &DomainError) -> String {
    err.to_string()
}

#[test]
fn a_cutover_instant_in_the_past_is_refused_at_submit() {
    let err = check_cutover_instant(now() - MAX_BATCHING_DELAY, now(), ChangeoverMoment::Submit)
        .expect_err("a past cutover instant must be refused");

    assert!(
        matches!(err, DomainError::CutoverInstantPassed(_)),
        "the cutover's own code, not the supersession's: {err:?}"
    );
    assert!(
        message(&err).contains("submit"),
        "the refusal names which floor was missed: {}",
        message(&err)
    );
}

#[test]
fn an_instant_inside_the_batching_delay_is_refused_at_commit() {
    // The whole reason the commit floor exists: an instant inside the batching
    // and warm lag activates the successor's window while its row is not yet
    // addressable at any completed `CatalogVersion`, so renewals and arrears on
    // the key just closed fail transiently.
    let inside = now() + MAX_BATCHING_DELAY - time::Duration::seconds(1);

    let err = check_cutover_instant(inside, now(), ChangeoverMoment::Commit)
        .expect_err("an instant inside the batching delay must be refused at commit");

    assert!(matches!(err, DomainError::CutoverInstantPassed(_)));
    assert!(
        message(&err).contains(&MAX_BATCHING_DELAY.whole_seconds().to_string()),
        "the remedy formats the constant rather than restating the number: {}",
        message(&err)
    );
}

#[test]
fn an_instant_clear_of_the_delay_commits() {
    assert!(
        check_cutover_instant(
            now() + MAX_BATCHING_DELAY + time::Duration::seconds(1),
            now(),
            ChangeoverMoment::Commit,
        )
        .is_ok()
    );
}

#[test]
fn the_two_units_share_one_floor_and_answer_two_codes() {
    // **§5 says this in as many words**: `SUPERSESSION_INSTANT_PASSED` is "the same
    // floor `inst-gc-compose` gives cutovers, applied to the everyday mechanism".
    // So the bound is one spelling — a second copy is how two mechanisms come to
    // disagree about one SLO — while the code stays each unit's own, because an
    // operator reading a refusal is told which act they were performing.
    let inside = now() + MAX_BATCHING_DELAY - time::Duration::seconds(1);

    let cutover = check_cutover_instant(inside, now(), ChangeoverMoment::Commit)
        .expect_err("the cutover refuses");
    let supersession = check_changeover_instant(inside, now(), ChangeoverMoment::Commit)
        .expect_err("the supersession refuses the same instant");

    assert!(matches!(cutover, DomainError::CutoverInstantPassed(_)));
    assert!(matches!(
        supersession,
        DomainError::SupersessionInstantPassed(_)
    ));
    assert_ne!(
        CUTOVER_INSTANT_PASSED, SUPERSESSION_INSTANT_PASSED,
        "two codes, and the design set declares both"
    );

    // And the sentence is one sentence, so a drift in the bound cannot show up in
    // one unit and not the other. Asserted on the **shared floor** rather than on
    // the two envelopes: each variant carries its own `#[error]` prefix, which is
    // exactly the part that is supposed to differ, so comparing whole messages
    // would be comparing the thing the test is not about.
    let as_cutover = changeover_floor(inside, now(), ChangeoverMoment::Commit, "cutover")
        .expect_err("the floor refuses");
    let as_changeover = changeover_floor(inside, now(), ChangeoverMoment::Commit, "changeover")
        .expect_err("the same floor, the same instant");
    assert_eq!(
        as_cutover.replacen("cutover", "changeover", 1),
        as_changeover,
        "one floor, one sentence: only the noun is the caller's"
    );

    // And both envelopes carry that sentence verbatim, so the shared floor is what
    // an operator actually reads in either unit.
    assert!(message(&cutover).contains(&as_cutover));
    assert!(message(&supersession).contains(&as_changeover));

    // **The noun reaches the sentence**, asserted separately because the equality
    // above cannot see it: `"changeover"` does not contain `"cutover"` as a
    // substring, so the normalisation is a no-op when the noun is ignored and the
    // comparison passes either way. Found by a probe that hard-coded the noun and
    // reddened nothing — the probe ran and compiled, so the empty failure list was
    // a fact about this test rather than about the probe.
    assert!(
        as_cutover.starts_with("cutover instant "),
        "the caller's noun is what the operator reads: {as_cutover}"
    );
    assert!(
        as_changeover.starts_with("changeover instant "),
        "and the supersession keeps its own: {as_changeover}"
    );
}

// ---------------------------------------------------------------------------
// The three window operations (`inst-co-shorten` / `inst-co-copy` / `inst-co-successor`)
// ---------------------------------------------------------------------------

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 6, hour, 0, 0)
}

fn window(
    id: u128,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
    state: WindowState,
) -> NamedWindow {
    NamedWindow {
        window_id: Uuid::from_u128(id),
        interval: WindowInterval::new(from, to, state),
    }
}

#[test]
fn the_three_operations_are_born_of_one_instant() {
    // The gap-freeness `inst-co-atomic` demands is by **construction**: all three
    // instants are the cutover, so no caller can compose a shorten to T1 with
    // schedules from T2 > T1 and leave `[T1, T2)` uncovered. That defect was real
    // on the supersession unit and was found by review, not by a gate — collision
    // is an intersection test and a gap is not an intersection.
    let plane = vec![window(1, at(1), None, WindowState::Active)];

    let composed = compose_cutover_windows(&plane, at(10)).expect("a live key composes");

    assert_eq!(composed.shorten().effective_to, at(10));
    assert_eq!(composed.shorten().window_id, Uuid::from_u128(1));
    assert_eq!(composed.copy().effective_from, at(10));
    assert_eq!(composed.successor().effective_from, at(10));
    assert_eq!(composed.copy().effective_to, None);
    assert_eq!(composed.successor().effective_to, None);
}

#[test]
fn a_dormant_key_is_refused_by_the_cutovers_own_code() {
    // `inst-co-shorten`: the unit presupposes coverage to shorten. Reviving a
    // dormant key is a publish plus a schedule, never a cutover — so the refusal
    // says that rather than inviting a retry.
    let plane = vec![window(1, at(1), Some(at(5)), WindowState::Active)];

    let err = compose_cutover_windows(&plane, at(10)).expect_err("a dormant key must be refused");

    assert!(matches!(err, DomainError::CutoverGap(_)), "{err:?}");
    assert!(
        message(&err).contains("dormant") && message(&err).contains(&format_rfc3339(at(10))),
        "the refusal names the instant with no coverage: {}",
        message(&err)
    );
}

#[test]
fn a_window_beginning_at_the_cutover_is_refused_rather_than_emptied() {
    // The other side of the same absence: shortening a window to its own start
    // leaves `[cutover, cutover)`, so there is no coverage to hand over. On the
    // supersession unit this arm was missing and the key was told it "carries later
    // coverage", naming a sibling that does not exist (review, 2026-08-05).
    let plane = vec![window(7, at(10), None, WindowState::Scheduled)];

    let err = compose_cutover_windows(&plane, at(10)).expect_err("an empty shorten is refused");

    assert!(matches!(err, DomainError::CutoverGap(_)), "{err:?}");
    assert!(
        message(&err).contains(&Uuid::from_u128(7).to_string()),
        "the refusal names the window the operator has to deal with: {}",
        message(&err)
    );
}

#[test]
fn later_coverage_the_cutover_would_not_replace_is_a_collision() {
    let plane = vec![
        window(1, at(1), None, WindowState::Active),
        window(2, at(20), None, WindowState::Scheduled),
    ];

    let err = compose_cutover_windows(&plane, at(10)).expect_err("the later window collides");

    assert!(matches!(err, DomainError::WindowOverlap(_)), "{err:?}");
    assert!(message(&err).contains(&Uuid::from_u128(2).to_string()));
}

#[test]
fn cancelled_and_expired_windows_are_not_coverage() {
    // The occupying set is the shared one, so a cancelled window neither covers the
    // cutover nor collides with the successor.
    let plane = vec![
        window(1, at(1), None, WindowState::Cancelled),
        window(2, at(20), None, WindowState::Cancelled),
    ];

    let err = compose_cutover_windows(&plane, at(10))
        .expect_err("a key whose only windows are cancelled is dormant");

    assert!(matches!(err, DomainError::CutoverGap(_)), "{err:?}");
}

// ---------------------------------------------------------------------------
// `inst-co-copy`: the grandfathered copy's new generation key
// ---------------------------------------------------------------------------

fn predecessor_key() -> ScopeKey {
    ScopeKey::new(
        PlanId::new(Uuid::from_u128(0x9_1a4)),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
}

#[test]
fn the_copy_lands_on_a_new_generation_of_the_predecessors_key() {
    // `inst-co-copy`: only the copy moves. Every axis but the two the generation
    // is made of is the predecessor's, or the copy would be filed under a key no
    // resolution class reaches from the predecessor's market.
    let copy = grandfathered_copy_key(&predecessor_key(), at(10), &[])
        .expect("a fresh generation is mintable");

    assert_eq!(
        copy.price_eligibility(),
        PriceEligibility::ExistingGrandfathered
    );
    assert_eq!(copy.cohort(), Cohort::Generation(at(10)));
    assert_eq!(copy.currency(), predecessor_key().currency());
    assert_eq!(copy.region(), predecessor_key().region());
    assert_eq!(copy.phase(), predecessor_key().phase());
    assert_eq!(copy.charge_kind(), predecessor_key().charge_kind());
    assert_ne!(copy, predecessor_key());
}

#[test]
fn the_copy_carries_the_predecessors_usage_line() {
    // D-196 made the line two axes of the key. A copy that dropped it would be
    // filed under the meterless line of the same market — a key the predecessor's
    // subscribers never resolve to, and one that silently collides with the copy
    // of a *different* meter's cutover on the same plan.
    let metered = predecessor_key()
        .with_usage_line(None, DimensionKey::none())
        .expect("the recurring key carries no line");
    let usage = ScopeKey::new(
        PlanId::new(Uuid::from_u128(0x9_1a4)),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Usage,
        Cohort::None,
    )
    .expect("key")
    .with_usage_line(
        Some(Meter::new("cloudlets").expect("a non-blank meter")),
        DimensionKey::new("region=eu"),
    )
    .expect("a usage key carries its line");

    let copy = grandfathered_copy_key(&usage, at(10), &[]).expect("a metered generation");

    assert_eq!(copy.meter().map(Meter::as_str), Some("cloudlets"));
    assert_eq!(copy.dimension_key().as_str(), "region=eu");

    // And the line-less predecessor's copy acquires no line, which is the
    // direction that would file the copy under a different tenth axis from the
    // key it was copied from.
    let line_less = grandfathered_copy_key(&metered, at(10), &[]).expect("a generation");

    assert_eq!(line_less.meter(), None);
    assert!(line_less.dimension_key().is_none());
}

#[test]
fn a_generation_that_already_exists_is_refused_at_compose() {
    // `inst-co-copy`: an instant equal to an existing generation's cohort is
    // refused here rather than at the store, because at the store it arrives as a
    // partial-UNIQUE violation inside the commit — a 500 for a request whose author
    // only has to move the instant.
    let err = grandfathered_copy_key(&predecessor_key(), at(10), &[Cohort::Generation(at(10))])
        .expect_err("a second generation on one instant must be refused");

    assert!(matches!(err, DomainError::DuplicateScopeKey(_)), "{err:?}");
    assert!(
        message(&err).contains(&format_rfc3339(at(10))),
        "the refusal names the instant already taken: {}",
        message(&err)
    );
}

#[test]
fn prior_generations_on_other_instants_are_untouched() {
    let copy = grandfathered_copy_key(
        &predecessor_key(),
        at(10),
        &[Cohort::Generation(at(3)), Cohort::Generation(at(7))],
    )
    .expect("prior generations do not block a new one");

    assert_eq!(copy.cohort(), Cohort::Generation(at(10)));
}

#[test]
fn the_copys_window_is_open_ended_so_the_d04_bound_holds_by_construction() {
    // `inst-co-bounds` (D-04): the copy carries two clocks, its window and
    // `grandfatherUntil`, and the window MUST cover through `grandfatherUntil` plus
    // **the longest billing cycle sold on that key** - open-ended when the horizon
    // is null. The margin exists because re-binding happens only at the next
    // renewal after expiry: a bound period that started before the horizon keeps
    // rating against the generation's key until that renewal, so a window ending at
    // `grandfatherUntil` strands subscribers for up to one full cycle.
    //
    // **Compose satisfies that bound the only way it can here: by not ending the
    // window at all.** An open interval covers every instant the rule could name,
    // whatever the horizon and whatever the cycle roster says - which matters
    // because the roster is not in hand at compose, and W6 leaves "the longest
    // billing cycle sold on the key" undefined for usage and one-time keys anyway.
    // Nothing leaks: the row sells nothing new past expiry, because new
    // subscriptions never bind grandfathered rows.
    //
    // So the rule's teeth are on the **adjustment** path - a later `effectiveTo`
    // below the bound - and not here. This case exists so that a future change
    // giving the copy a computed end has to come past it and answer the bound
    // explicitly rather than by omission.
    //
    // **It adds no coverage of its own.** The open end is already written where the
    // tree can check it: `the_three_operations_are_born_of_one_instant` asserts
    // `composed.copy().effective_to == None`, and a change giving the copy a
    // computed end reddens both. What this case
    // adds is the **rule's name and argument** at the assertion, so an author making
    // that change meets D-04 rather than an equality they will read as incidental -
    // which is worth a duplicated assertion, and is a different claim from the one
    // that was made for it.
    let plane = vec![window(1, at(1), None, WindowState::Active)];

    let composed = compose_cutover_windows(&plane, at(10)).expect("a live key composes");

    assert_eq!(
        composed.copy().effective_to,
        None,
        "the grandfathered copy's window is open-ended at compose, which is what makes the \
         D-04 bound hold whatever the horizon and the cycle roster turn out to be"
    );
}

// ---------------------------------------------------------------------------
// The act's identity and the keys it holds (`inst-gc-api`, `inst-co-single-pending`)
// ---------------------------------------------------------------------------

/// A second selectable key on the predecessor's plan: another market.
fn sibling_market_key() -> ScopeKey {
    ScopeKey::new(
        predecessor_key().plan_id(),
        CurrencyCode::new("USD").expect("three letters"),
        Region::new("US").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
}

#[test]
fn two_selections_at_one_instant_are_two_acts() {
    // D-28: the act is `(planId, key-set hash, cutover instant)`. A selection is
    // *what is being cut over*, so narrowing or widening it is a different request
    // and must not be answered by the unit already standing for the other one. With
    // the selection out of the name, a retry that dropped a key would find the wider
    // unit by subject, be reported `submitted`, and an approver would authorize the
    // key the operator had just removed.
    let plan = predecessor_key().plan_id();

    let one_key = crate::infra::cutover::cutover_unit_ref(plan, &[predecessor_key()], at(10));
    let two_keys = crate::infra::cutover::cutover_unit_ref(
        plan,
        &[predecessor_key(), sibling_market_key()],
        at(10),
    );

    assert_ne!(
        one_key, two_keys,
        "one key and two are two different acts at one instant"
    );
}

#[test]
fn one_selection_named_in_any_order_is_one_act() {
    // The other half, and the half that makes the hash a *set* hash: the selector is
    // a set, so two orderings of one selection are one act and a retry of it finds
    // the unit it opened. A repeated key is the same selection named twice.
    let plan = predecessor_key().plan_id();

    let forwards = crate::infra::cutover::cutover_unit_ref(
        plan,
        &[predecessor_key(), sibling_market_key()],
        at(10),
    );
    let backwards = crate::infra::cutover::cutover_unit_ref(
        plan,
        &[sibling_market_key(), predecessor_key()],
        at(10),
    );
    let repeated = crate::infra::cutover::cutover_unit_ref(
        plan,
        &[predecessor_key(), sibling_market_key(), predecessor_key()],
        at(10),
    );

    assert_eq!(forwards, backwards, "a set has no order");
    assert_eq!(forwards, repeated, "a set holds a key once");
}

#[test]
fn the_act_names_the_plan_the_selection_and_the_instant() {
    // The two remaining discriminators, and the shape a store and an operator read.
    let plan = predecessor_key().plan_id();
    let selection = [predecessor_key()];

    let one = crate::infra::cutover::cutover_unit_ref(plan, &selection, at(10));
    let other_instant = crate::infra::cutover::cutover_unit_ref(plan, &selection, at(11));

    assert_ne!(one, other_instant, "two dates are two acts");
    assert!(one.contains("/cutover/"), "the act names its kind: {one}");
    assert!(
        one.starts_with(&format!("{}/", plan.get())),
        "`subject_aggregate` refuses a subject that is not `<plan_id>/<rest>`: {one}"
    );
}

#[test]
fn two_usage_lines_of_one_market_are_two_selections() {
    // The key-set hash is taken over the *canonical* rendering, so it discriminates
    // on all ten axes rather than on the eight a hand-written encoding would list.
    // Two meters of one market are two selections, which is the same fact C1 had to
    // restate for the sellability gate.
    let plan = predecessor_key().plan_id();
    let line = |meter: &str| {
        ScopeKey::new(
            plan,
            CurrencyCode::new("EUR").expect("three letters"),
            Region::new("EU").expect("a non-blank region"),
            PhaseId::new(Uuid::from_u128(0xfa_5e)),
            PriceEligibility::AllSubscriptions,
            ChargeKind::Usage,
            Cohort::None,
        )
        .expect("all_subscriptions pairs with cohort none")
        .with_usage_line(
            Some(Meter::new(meter).expect("a non-blank meter")),
            DimensionKey::none(),
        )
        .expect("a usage row carries a usage line")
    };

    assert_ne!(
        crate::infra::cutover::cutover_unit_ref(plan, &[line("api-calls")], at(10)),
        crate::infra::cutover::cutover_unit_ref(plan, &[line("storage-gb")], at(10)),
        "two meters of one market are two acts"
    );
}

#[test]
fn a_cutover_holds_the_market_key_and_the_new_generation() {
    // `inst-co-single-pending`: a cutover unit pends **both** keys it touches.
    // Without the first, a window mutation could be approved against the key
    // mid-cutover; without the second, two cutovers of one plan at one instant would
    // each read the generation as free and the loser would meet a partial-UNIQUE
    // violation inside its commit instead of a refusal at submit.
    let held = crate::infra::cutover::cutover_held_keys(&predecessor_key(), at(10), &[])
        .expect("both keys are mintable");

    assert_eq!(held[0], predecessor_key());
    assert_eq!(held[1].cohort(), Cohort::Generation(at(10)));
    assert_eq!(
        held[1].price_eligibility(),
        PriceEligibility::ExistingGrandfathered
    );
    assert_ne!(held[0], held[1], "two keys, not one named twice");
}

#[test]
fn a_cutover_onto_an_existing_generation_holds_nothing() {
    // The refusal arrives here rather than at the store, so the caller never gets a
    // half-held pair: `cutover_held_keys` mints the second key through the same door
    // that refuses a duplicate generation.
    let err = crate::infra::cutover::cutover_held_keys(
        &predecessor_key(),
        at(10),
        &[Cohort::Generation(at(10))],
    )
    .expect_err("the generation is taken");

    assert!(matches!(err, DomainError::DuplicateScopeKey(_)), "{err:?}");
}
