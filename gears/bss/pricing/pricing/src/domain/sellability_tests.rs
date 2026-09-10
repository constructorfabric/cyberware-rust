//! What each predicate answers, and the world in which that predicate is the
//! thing answering.
//!
//! **Every instant here is a parameter, not a clock.** `at(day)` is a fixed
//! instant and the `at` every case evaluates against is passed in, so no case can
//! start passing or failing because today's date moved past it — which is the
//! whole property that makes a frozen version answer a past order instant the same
//! way forever.
//!
//! The staging discipline these cases follow: each starts from `sellable_facts()`,
//! a world in which every answerable predicate is satisfied, and changes **one**
//! fact. So the predicate under test is the one that answers, and `only_failure_is`
//! names the others as still satisfied — the observability twin a refusal needs,
//! because a rule that refused everything would pass its own refusal test.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing_sdk::CatalogVersion;

use super::{
    KeySellability, PinnedFacts, PlanMarketVerdict, Predicate, PredicateAnswer, PredicateOutcome,
    SellabilityFacts, SellabilitySurface,
};
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::CurrencyCode;
use crate::domain::plan_shape::Frequency;
use crate::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::window::{CoverageEnd, KeyWindows, WindowInterval, WindowState};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A window-scale instant: 2099-01-01 plus `day` whole days.
///
/// 2099 for the suite convention's reason and not because anything here reads a
/// clock: every instant in this file is compared against another instant in this
/// file.
fn at(day: i64) -> OffsetDateTime {
    utc_ymd_hms(2099, 1, 1, 0, 0, 0) + time::Duration::days(day)
}

fn plan() -> PlanId {
    PlanId::new(uuid::Uuid::from_u128(0x5e_11))
}

fn phase() -> PhaseId {
    PhaseId::new(uuid::Uuid::from_u128(0xfa_5e))
}

fn eur() -> CurrencyCode {
    CurrencyCode::new("EUR").expect("three letters")
}

fn eu() -> Region {
    Region::new("eu").expect("a non-blank region")
}

fn usd() -> CurrencyCode {
    CurrencyCode::new("USD").expect("three letters")
}

fn key_of(charge_kind: ChargeKind, currency: &CurrencyCode, region: &Region) -> ScopeKey {
    ScopeKey::new(
        plan(),
        currency.clone(),
        region.clone(),
        phase(),
        PriceEligibility::AllSubscriptions,
        charge_kind,
        Cohort::None,
    )
    .expect("the class pairs with cohort none")
}

fn recurring() -> ScopeKey {
    key_of(ChargeKind::Recurring, &eur(), &eu())
}

fn usage() -> ScopeKey {
    key_of(ChargeKind::Usage, &eur(), &eu())
}

/// A recurring key on the baseline market under another eligibility class.
fn eligibility_of(class: PriceEligibility, cohort: Cohort) -> ScopeKey {
    ScopeKey::new(
        plan(),
        eur(),
        eu(),
        phase(),
        class,
        ChargeKind::Recurring,
        cohort,
    )
    .expect("the class pairs with the cohort")
}

/// One usage **line** on the baseline market: a meter, a dimension, a class.
///
/// The pair D-196 added, which `eligibility_of` cannot express: it builds a
/// recurring key, and a recurring key carrying a meter is refused by
/// `check_usage_line_axes`.
fn usage_line_of(meter: &str, dimension: &str, class: PriceEligibility) -> ScopeKey {
    ScopeKey::new(
        plan(),
        eur(),
        eu(),
        phase(),
        class,
        ChargeKind::Usage,
        Cohort::None,
    )
    .expect("the class pairs with cohort none")
    .with_usage_line(
        Some(Meter::new(meter).expect("a non-blank meter")),
        DimensionKey::new(dimension),
    )
    .expect("a usage row carries a usage line")
}

fn windows_of(scope_key: ScopeKey, intervals: Vec<WindowInterval>) -> KeyWindows {
    KeyWindows {
        scope_key,
        intervals,
    }
}

/// A world in which every answerable predicate is satisfied at `at(10)`: a
/// published, monthly, in-dates plan whose one recurring key is covered from
/// `at(0)` open-ended.
fn sellable_facts() -> PinnedFacts {
    PinnedFacts {
        plan_id: plan(),
        catalog_version: CatalogVersion::new(7),
        lifecycle_state: LifecycleState::Published,
        available_from: Some(at(0)),
        available_to: None,
        frequency: Some(Frequency::Monthly),
        price_keys: vec![recurring()],
        windows: vec![windows_of(
            recurring(),
            vec![WindowInterval::new(at(0), None, WindowState::Active)],
        )],
    }
}

/// The surface over `facts`, evaluated at `at(10)` on the `EUR`/`eu` market.
fn surface_of(facts: PinnedFacts) -> SellabilitySurface {
    SellabilitySurface::of_delta(&SellabilityFacts::Pinned(facts), at(10), &eur(), &eu())
}

/// The answer a named predicate gave, plan-level or per key.
fn answer(surface: &SellabilitySurface, predicate: Predicate) -> &PredicateAnswer {
    surface
        .plan_answers
        .iter()
        .chain(surface.keys.iter().flat_map(|key| key.answers.iter()))
        .find(|outcome| outcome.predicate == predicate)
        .map(|outcome| &outcome.answer)
        .expect("every predicate is answered somewhere on the surface")
}

/// One key's answer to a per-key predicate.
fn key_answer(key: &KeySellability, predicate: Predicate) -> &PredicateAnswer {
    key.answers
        .iter()
        .find(|outcome| outcome.predicate == predicate)
        .map(|outcome| &outcome.answer)
        .expect("the key answers every per-key predicate")
}

/// Assert every predicate this build can evaluate, other than `except`, is still
/// satisfied — the observability twin.
fn only_failure_is(surface: &SellabilitySurface, except: Predicate) {
    for predicate in Predicate::ALL {
        if *predicate == except
            || matches!(
                predicate,
                Predicate::GaGateFlags | Predicate::RegistrySellable
            )
        {
            continue;
        }
        assert_eq!(
            answer(surface, *predicate),
            &PredicateAnswer::Satisfied,
            "predicate ({}) must still be satisfied, so the one under test is what answered",
            predicate.ordinal()
        );
    }
}

// ---------------------------------------------------------------------------
// The baseline every case below changes one fact of
// ---------------------------------------------------------------------------

#[test]
fn the_baseline_world_fails_no_predicate_it_can_evaluate() {
    // Without this, every "one fact changed" case below could be passing because
    // the baseline was already refusing for some other reason.
    let surface = surface_of(sellable_facts());

    assert_eq!(
        answer(&surface, Predicate::ActiveWindowWithHorizon),
        &PredicateAnswer::Satisfied
    );
    assert_eq!(
        answer(&surface, Predicate::CommittedVersion),
        &PredicateAnswer::Satisfied
    );
    assert_eq!(
        answer(&surface, Predicate::AvailabilityDates),
        &PredicateAnswer::Satisfied
    );
    assert_eq!(
        answer(&surface, Predicate::PlanLifecycleState),
        &PredicateAnswer::Satisfied
    );
}

// ---------------------------------------------------------------------------
// Predicate (1) — the half this slice owns
// ---------------------------------------------------------------------------

#[test]
fn a_scheduled_only_key_is_not_sellable() {
    // The key's one window starts *after* the instant asked about, which is what
    // "scheduled but not active" is as an interval: nothing is effective at
    // `at(10)`, so no sale may bind there.
    let facts = PinnedFacts {
        windows: vec![windows_of(
            recurring(),
            vec![WindowInterval::new(at(20), None, WindowState::Scheduled)],
        )],
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    assert!(matches!(
        key_answer(&surface.keys[0], Predicate::ActiveWindowWithHorizon),
        PredicateAnswer::Failed { .. }
    ));
    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotSellable
    );
    only_failure_is(&surface, Predicate::ActiveWindowWithHorizon);
}

#[test]
fn a_window_the_sweep_has_not_flipped_is_active_from_its_interval_not_its_token() {
    // The settled reading of the state token, and the case that makes it
    // load-bearing: the interval covers `at(10)` while the token still says
    // `scheduled`, because D-99 makes an activation re-project nothing. A predicate
    // that branched on the token would read this key unsellable forever, and the
    // activation the sweep performed would owe a re-projection of a store whose
    // contract is that a completed version never changes.
    let facts = PinnedFacts {
        windows: vec![windows_of(
            recurring(),
            vec![WindowInterval::new(at(0), None, WindowState::Scheduled)],
        )],
        ..sellable_facts()
    };

    assert_eq!(
        key_answer(
            &surface_of(facts).keys[0],
            Predicate::ActiveWindowWithHorizon
        ),
        &PredicateAnswer::Satisfied,
        "active-at-t is derived from `interval AND at`, never read off the state token"
    );
}

#[test]
fn a_key_whose_coverage_ends_inside_the_horizon_is_not_sellable() {
    // The D-80 coverage horizon. The key is covered *at* `at(10)` and its coverage
    // ends at `at(20)` — inside `at(10) + 31 days`, the monthly margin rounded up —
    // so a subscription bought at `at(10)` would run out of coverage inside its
    // first billing cycle. That is the trailing void nobody may buy into.
    let facts = PinnedFacts {
        windows: vec![windows_of(
            recurring(),
            vec![WindowInterval::new(
                at(0),
                Some(at(20)),
                WindowState::Active,
            )],
        )],
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    let key = &surface.keys[0];
    assert_eq!(key.coverage_end, CoverageEnd::Ends(at(20)));
    assert!(matches!(
        key_answer(key, Predicate::ActiveWindowWithHorizon),
        PredicateAnswer::Failed { .. }
    ));
    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotSellable
    );
    only_failure_is(&surface, Predicate::ActiveWindowWithHorizon);
}

#[test]
fn coverage_reaching_exactly_the_horizon_satisfies_it() {
    // The boundary of the same rule, from the other side, so the margin is
    // asserted as a `>=` rather than as "some number of days". A month rounds
    // **up** to 31 days (`cycle_length`), because a margin rounded down leaves the
    // tail of a bound period uncovered.
    let facts = PinnedFacts {
        windows: vec![windows_of(
            recurring(),
            vec![WindowInterval::new(
                at(0),
                Some(at(10) + time::Duration::days(31)),
                WindowState::Active,
            )],
        )],
        ..sellable_facts()
    };

    assert_eq!(
        key_answer(
            &surface_of(facts).keys[0],
            Predicate::ActiveWindowWithHorizon
        ),
        &PredicateAnswer::Satisfied
    );
}

#[test]
fn an_open_ended_window_satisfies_the_horizon() {
    // `CoverageEnd::OpenEnded` passes trivially, and the arm exists because an
    // `Option<DateTime>` could not have told this apart from `Uncovered` — two
    // answers that are opposites under this very predicate.
    let surface = surface_of(sellable_facts());

    let key = &surface.keys[0];
    assert_eq!(key.coverage_end, CoverageEnd::OpenEnded);
    assert_eq!(
        key_answer(key, Predicate::ActiveWindowWithHorizon),
        &PredicateAnswer::Satisfied
    );
}

#[test]
fn a_plan_with_no_recurring_part_has_a_zero_margin() {
    // W6's zero margin: on a plan with no recurring row on the market the horizon
    // reduces to "a window covers `t`". The key here is `usage`, its coverage ends
    // one day after the instant asked about, and that is enough — a one-time or
    // usage purchase needs no forward coverage. Under a monthly margin the same
    // shape fails, which is what makes the zero the thing being asserted.
    let facts = PinnedFacts {
        price_keys: vec![usage()],
        windows: vec![windows_of(
            usage(),
            vec![WindowInterval::new(
                at(0),
                Some(at(11)),
                WindowState::Active,
            )],
        )],
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    assert_eq!(
        key_answer(&surface.keys[0], Predicate::ActiveWindowWithHorizon),
        &PredicateAnswer::Satisfied
    );
}

#[test]
fn the_same_shape_on_a_recurring_market_fails_the_monthly_margin() {
    // The twin of the zero-margin case: identical coverage, one axis changed from
    // `usage` to `recurring`, and the margin becomes a month. Without it, the case
    // above would hold for a horizon that was never applied at all.
    let facts = PinnedFacts {
        windows: vec![windows_of(
            recurring(),
            vec![WindowInterval::new(
                at(0),
                Some(at(11)),
                WindowState::Active,
            )],
        )],
        ..sellable_facts()
    };

    assert!(matches!(
        key_answer(
            &surface_of(facts).keys[0],
            Predicate::ActiveWindowWithHorizon
        ),
        PredicateAnswer::Failed { .. }
    ));
}

#[test]
fn a_recurring_market_with_no_authored_frequency_refuses_rather_than_reading_zero() {
    // W6's third answer. `longest_cycle_sold_on` returns `None` — the market sells
    // recurring and the plan authored no frequency — so the term has **no value**,
    // which is a different fact from `Some(zero)`. Folding the two would make this
    // key sellable on a plan whose billing cycle nobody declared: the direction a
    // fail-closed gate must never round in, and the same arm
    // `infra::window::refuse_trailing_void` refuses on.
    //
    // It is `Failed` and not `NotEvaluable`: what is missing is authored data on a
    // plan this version froze, not a slice that has not landed, and a consumer told
    // "not evaluable" may conclude the gate is not yet a gate and proceed.
    let facts = PinnedFacts {
        frequency: None,
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    let given = key_answer(&surface.keys[0], Predicate::ActiveWindowWithHorizon);
    assert!(
        matches!(given, PredicateAnswer::Failed { detail } if detail.contains("no frequency")),
        "an unknown margin refuses and names why: {given:?}"
    );
    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotSellable
    );
}

#[test]
fn the_same_key_is_satisfied_with_a_frequency_authored() {
    // The twin of the `None`-margin refusal: identical world, frequency restored,
    // and the key passes. Without it, `frequency: None` failing would be consistent
    // with a predicate that refuses every input.
    assert_eq!(
        key_answer(
            &surface_of(sellable_facts()).keys[0],
            Predicate::ActiveWindowWithHorizon
        ),
        &PredicateAnswer::Satisfied
    );
}

#[test]
fn an_instant_whose_horizon_is_not_representable_refuses_rather_than_panicking() {
    // `at` is **request input** on this surface, unlike the mutation path's `now`,
    // so a caller can name an instant for which `at + margin` is not a
    // representable timestamp. Adding it unchecked would panic inside a read
    // handler; skipping the horizon would let that same caller round the gate. The
    // fail-closed answer is the false one, and it names why.
    let surface = SellabilitySurface::of_delta(
        &SellabilityFacts::Pinned(sellable_facts()),
        crate::domain::instant::max_utc(),
        &eur(),
        &eu(),
    );

    let given = key_answer(&surface.keys[0], Predicate::ActiveWindowWithHorizon);
    assert!(
        matches!(given, PredicateAnswer::Failed { detail } if detail.contains("representable")),
        "an unrepresentable horizon refuses: {given:?}"
    );
}

#[test]
fn a_key_with_no_surviving_window_reads_uncovered_and_refuses() {
    // A projected key whose windows were all cancelled, or that never had one, is
    // present on the surface with an empty interval list and an `Uncovered`
    // coverage end - "this key is uncovered" having one declared spelling rather
    // than being inferred from an absent entry (D-167 clause 1).
    let facts = PinnedFacts {
        windows: vec![windows_of(recurring(), Vec::new())],
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    let key = &surface.keys[0];
    assert_eq!(key.coverage_end, CoverageEnd::Uncovered);
    assert!(key.intervals.is_empty());
    assert!(matches!(
        key_answer(key, Predicate::ActiveWindowWithHorizon),
        PredicateAnswer::Failed { .. }
    ));
}

// ---------------------------------------------------------------------------
// Predicates (2)(3)(4) — the ones a version already answers
// ---------------------------------------------------------------------------

#[test]
fn a_pending_uncommitted_version_is_not_sellable() {
    // Predicate (2). No committed, pin-eligible version carries the plan's subject,
    // so its content is not addressable from any pin — the "pending fan-out is NOT
    // sellable" half of `inst-sg-surface`.
    //
    // The other predicates answer `NotEvaluable` rather than `Failed`, and that is
    // the point of the arm: with no version there is no fact for them to read, so
    // calling them *false* would be inventing an answer about a plan nobody can
    // see.
    let surface = SellabilitySurface::of_delta(
        &SellabilityFacts::NotAddressable { plan_id: plan() },
        at(10),
        &eur(),
        &eu(),
    );

    assert!(matches!(
        answer(&surface, Predicate::CommittedVersion),
        PredicateAnswer::Failed { .. }
    ));
    for predicate in [Predicate::AvailabilityDates, Predicate::PlanLifecycleState] {
        assert!(
            matches!(
                answer(&surface, predicate),
                PredicateAnswer::NotEvaluable { .. }
            ),
            "predicate ({}) has no operand without a version",
            predicate.ordinal()
        );
    }
    assert_eq!(surface.catalog_version, None);
    assert!(surface.keys.is_empty());
    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotSellable
    );
}

#[test]
fn a_committed_version_answers_predicate_two_satisfied_and_names_itself() {
    // The twin. Same market, same instant, one fact changed — the plan resolves to
    // a committed version — and predicate (2) flips. Without this, the refusal
    // above would be consistent with a predicate that is never satisfied.
    let surface = surface_of(sellable_facts());

    assert_eq!(
        answer(&surface, Predicate::CommittedVersion),
        &PredicateAnswer::Satisfied
    );
    assert_eq!(surface.catalog_version, Some(CatalogVersion::new(7)));
}

#[test]
fn an_out_of_dates_plan_is_not_sellable() {
    // Predicate (3), on the `availableTo` side, read half-open: `at(10)` is not
    // before `at(10)`, so the boundary instant is outside. Everything else in the
    // world is untouched and still satisfied.
    let facts = PinnedFacts {
        available_to: Some(at(10)),
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    assert!(matches!(
        answer(&surface, Predicate::AvailabilityDates),
        PredicateAnswer::Failed { .. }
    ));
    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotSellable
    );
    only_failure_is(&surface, Predicate::AvailabilityDates);
}

#[test]
fn a_plan_not_yet_available_is_not_sellable_either() {
    // The other side of the same predicate. `availableFrom` after the instant asked
    // about refuses, so the rule is a window rather than a one-sided bound — and
    // the `availableFrom` half is not dead.
    let facts = PinnedFacts {
        available_from: Some(at(20)),
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    assert!(matches!(
        answer(&surface, Predicate::AvailabilityDates),
        PredicateAnswer::Failed { .. }
    ));
    only_failure_is(&surface, Predicate::AvailabilityDates);
}

#[test]
fn the_available_from_boundary_is_inside_the_window_and_the_quantum_before_it_is_not() {
    // Predicate (3)'s **start** boundary, which the case above stages ten days away
    // from and therefore does not hold. The module declares the reading
    // `[from, to)`, and `an_out_of_dates_plan_is_not_sellable` pins only the
    // `availableTo` end at its edge: flipping `at < from` to `at <= from` - making
    // the start exclusive - left the whole suite green, and would have refused a
    // legitimate first sale at exactly the instant the plan becomes purchasable. No
    // document of the design set settles inclusivity, which is why the convention
    // chosen here needs pinning rather than describing.
    //
    // The boundary is **one** value, bound once and passed to both the fact and the
    // question, so the case cannot drift into testing a neighbourhood of it.
    let from = at(0);
    let facts = PinnedFacts {
        available_from: Some(from),
        ..sellable_facts()
    };

    let surface = SellabilitySurface::of_delta(
        &SellabilityFacts::Pinned(facts.clone()),
        from,
        &eur(),
        &eu(),
    );

    assert_eq!(
        answer(&surface, Predicate::AvailabilityDates),
        &PredicateAnswer::Satisfied,
        "the start is inclusive: a purchase at `availableFrom` is inside the window"
    );

    // One D-144 quantum earlier is outside it, so what is asserted above is the edge
    // itself rather than a side of it. Only predicate (3) is read here - the window
    // does not cover that instant either, and (1) refusing is not the statement.
    let before = SellabilitySurface::of_delta(
        &SellabilityFacts::Pinned(facts),
        from - time::Duration::milliseconds(1),
        &eur(),
        &eu(),
    );
    assert!(matches!(
        answer(&before, Predicate::AvailabilityDates),
        PredicateAnswer::Failed { .. }
    ));
}

#[test]
fn a_retired_plan_is_not_sellable() {
    // Predicate (4), and the whole of D-128: the state is a **projected** field, so
    // a retired plan — which can never publish again, and would therefore never be
    // re-projected — is refused off the frozen version rather than advertised
    // sellable forever.
    let facts = PinnedFacts {
        lifecycle_state: LifecycleState::Retired,
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    let given = answer(&surface, Predicate::PlanLifecycleState);
    assert!(
        matches!(given, PredicateAnswer::Failed { detail } if detail.contains("retired")),
        "the refusal names the state: {given:?}"
    );
    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotSellable
    );
    only_failure_is(&surface, Predicate::PlanLifecycleState);
}

// ---------------------------------------------------------------------------
// D-94 — the conjunction
// ---------------------------------------------------------------------------

#[test]
fn one_failing_component_key_makes_the_plan_market_not_sellable() {
    // A hybrid: the recurring key is covered open-ended and passes, the usage key
    // is covered only to `at(20)` and fails its horizon. D-94 — never partially
    // sellable — makes the plan-market not sellable even so, and the two keys'
    // answers are asserted separately so the case cannot pass by both failing.
    let facts = PinnedFacts {
        price_keys: vec![recurring(), usage()],
        windows: vec![
            windows_of(
                recurring(),
                vec![WindowInterval::new(at(0), None, WindowState::Active)],
            ),
            windows_of(
                usage(),
                vec![WindowInterval::new(
                    at(0),
                    Some(at(20)),
                    WindowState::Active,
                )],
            ),
        ],
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    assert_eq!(surface.keys.len(), 2, "both component keys are gate inputs");
    let recurring_key = surface
        .keys
        .iter()
        .find(|key| key.scope_key.charge_kind() == ChargeKind::Recurring)
        .expect("the recurring key is on the surface");
    let usage_key = surface
        .keys
        .iter()
        .find(|key| key.scope_key.charge_kind() == ChargeKind::Usage)
        .expect("the usage key is on the surface");

    assert_eq!(
        key_answer(recurring_key, Predicate::ActiveWindowWithHorizon),
        &PredicateAnswer::Satisfied,
        "the recurring key is fully covered and must pass"
    );
    assert!(matches!(
        key_answer(usage_key, Predicate::ActiveWindowWithHorizon),
        PredicateAnswer::Failed { .. }
    ));
    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotSellable
    );
}

#[test]
fn the_conjunction_is_eligibility_resolved() {
    // Two resolutions in one world. The `existing_grandfathered` generation is
    // never a gate input — nobody new binds one — and where a
    // `new_subscriptions_only` sibling exists it wins over `all_subscriptions`
    // (most-specific-wins, W3). Both losing keys are staged **uncovered**, so if
    // either reached the gate the verdict would flip: the exclusion is what is
    // asserted, not merely the roster's length.
    let grandfathered = eligibility_of(
        PriceEligibility::ExistingGrandfathered,
        Cohort::Generation(at(1)),
    );
    let newcomers = eligibility_of(PriceEligibility::NewSubscriptionsOnly, Cohort::None);
    let everyone = recurring();

    let facts = PinnedFacts {
        price_keys: vec![everyone.clone(), newcomers.clone(), grandfathered.clone()],
        windows: vec![
            windows_of(everyone, Vec::new()),
            windows_of(
                newcomers.clone(),
                vec![WindowInterval::new(at(0), None, WindowState::Active)],
            ),
            windows_of(grandfathered, Vec::new()),
        ],
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    assert_eq!(
        surface
            .keys
            .iter()
            .map(|key| key.scope_key.clone())
            .collect::<Vec<_>>(),
        vec![newcomers],
        "the grandfathered generation and the shadowed all_subscriptions row are not gate inputs"
    );
    assert_eq!(
        key_answer(&surface.keys[0], Predicate::ActiveWindowWithHorizon),
        &PredicateAnswer::Satisfied
    );
}

#[test]
fn two_meters_of_one_market_are_not_siblings() {
    // Most-specific-wins ranks keys that compete for **one** sale, and two meters
    // of one market do not: a purchase binds both lines, so a `new_subscriptions_only`
    // row on `api-calls` says nothing about who may buy `storage-gb`. The
    // `storage-gb` line is staged **uncovered**, so a resolution that mistook the
    // two for siblings would drop it from the roster and answer over a key with no
    // window — which is the plan D-196 exists to make storable.
    let newcomers_line = usage_line_of("api-calls", "", PriceEligibility::NewSubscriptionsOnly);
    let everyone_line = usage_line_of("storage-gb", "", PriceEligibility::AllSubscriptions);

    let facts = PinnedFacts {
        price_keys: vec![newcomers_line.clone(), everyone_line.clone()],
        windows: vec![
            windows_of(
                newcomers_line,
                vec![WindowInterval::new(at(0), None, WindowState::Active)],
            ),
            windows_of(everyone_line, Vec::new()),
        ],
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    assert_eq!(
        surface.keys.len(),
        2,
        "both meters' lines are gate inputs: {:?}",
        surface
            .keys
            .iter()
            .map(|key| key.scope_key.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotSellable,
        "the uncovered line is what answers, and a failed predicate outranks the two unevaluable \
         ones"
    );
}

#[test]
fn two_dimensions_of_one_meter_are_not_siblings() {
    // The tenth axis on its own, with the ninth held equal: one meter, two
    // dimensions. Without this case the one above would still hold if `siblings`
    // compared `meter` and stopped there, which is exactly the partial fix the
    // ten-axis key invites.
    let newcomers_line = usage_line_of(
        "api-calls",
        "eu-west",
        PriceEligibility::NewSubscriptionsOnly,
    );
    let everyone_line = usage_line_of("api-calls", "eu-east", PriceEligibility::AllSubscriptions);

    let facts = PinnedFacts {
        price_keys: vec![newcomers_line.clone(), everyone_line.clone()],
        windows: vec![
            windows_of(
                newcomers_line,
                vec![WindowInterval::new(at(0), None, WindowState::Active)],
            ),
            windows_of(everyone_line, Vec::new()),
        ],
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    assert_eq!(
        surface.keys.len(),
        2,
        "both dimensions of the one meter are gate inputs: {:?}",
        surface
            .keys
            .iter()
            .map(|key| key.scope_key.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotSellable
    );
}

#[test]
fn a_grandfathered_generation_is_not_a_gate_input_even_with_no_sibling() {
    // The exclusion on its own, with nothing for most-specific-wins to resolve
    // against: a market whose only key is a grandfathered generation binds no
    // purchasable key at all. Without this case the test above would still hold if
    // `existing_grandfathered` were merely *ranked below* the newcomers row rather
    // than excluded.
    let grandfathered = eligibility_of(
        PriceEligibility::ExistingGrandfathered,
        Cohort::Generation(at(1)),
    );
    let facts = PinnedFacts {
        price_keys: vec![grandfathered.clone()],
        windows: vec![windows_of(
            grandfathered,
            vec![WindowInterval::new(at(0), None, WindowState::Active)],
        )],
        ..sellable_facts()
    };

    let surface = surface_of(facts);

    assert!(surface.keys.is_empty());
    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotSellable
    );
}

#[test]
fn a_market_the_plan_publishes_no_key_on_is_not_sellable() {
    // The conjunction over an empty set must not answer `true`. A purchase on a
    // market the plan prices nothing for would bind no line at all, so the empty
    // roster is a refusal rather than a vacuous pass — and the same world on the
    // market the plan *does* publish on is the baseline case above.
    let surface = SellabilitySurface::of_delta(
        &SellabilityFacts::Pinned(sellable_facts()),
        at(10),
        &usd(),
        &eu(),
    );

    assert!(surface.keys.is_empty());
    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotSellable
    );
}

// ---------------------------------------------------------------------------
// D-167 clause (3) — false is not unevaluable
// ---------------------------------------------------------------------------

#[test]
fn predicates_five_and_six_answer_not_evaluable_and_name_their_slices() {
    // The difference between "this predicate is false" and "this version cannot
    // evaluate this predicate", stated at the surface rather than discovered by the
    // first consumer. Each names what owes it: Slice 4 / Slice 10 for the GA-gate
    // flags, the registry gear for D-46's `sellable`.
    let surface = surface_of(sellable_facts());

    let ga = key_answer(&surface.keys[0], Predicate::GaGateFlags);
    assert!(
        matches!(ga, PredicateAnswer::NotEvaluable { owed_to }
            if owed_to.contains("Slice 4") && owed_to.contains("Slice 10")),
        "predicate (5) names the two slices that owe it: {ga:?}"
    );

    let registry = answer(&surface, Predicate::RegistrySellable);
    assert!(
        matches!(registry, PredicateAnswer::NotEvaluable { owed_to }
            if owed_to.contains("D-46") && owed_to.contains("registry")),
        "predicate (6) names the registry gear: {registry:?}"
    );
}

#[test]
fn the_verdict_is_never_sellable_while_two_predicates_are_not_evaluable() {
    // `inst-sg-pinned` is a claim about the *finished* gear, and this build is not
    // it: with (5) and (6) unevaluable on every version this gear can project, the
    // conjunction cannot reach `Sellable`. The verdict is `NotEvaluable` — a gate
    // saying it is not yet a gate — and a consumer must not read that as a yes.
    let surface = surface_of(sellable_facts());

    assert_eq!(
        surface.plan_market_verdict(),
        PlanMarketVerdict::NotEvaluable,
        "nothing failed, and two predicates have no operand"
    );
}

#[test]
fn a_failed_predicate_outranks_an_unevaluable_one() {
    // The order of the two arms, which is the whole fail-closed direction: with a
    // retired plan *and* two unevaluable predicates the answer is `NotSellable`,
    // not `NotEvaluable`. A gate reporting "cannot decide" where it holds a
    // definite refusal invites a consumer to proceed.
    let facts = PinnedFacts {
        lifecycle_state: LifecycleState::Retired,
        ..sellable_facts()
    };

    assert_eq!(
        surface_of(facts).plan_market_verdict(),
        PlanMarketVerdict::NotSellable
    );
}

// ---------------------------------------------------------------------------
// The roster, and the shape of the surface itself
// ---------------------------------------------------------------------------

#[test]
fn every_predicate_is_answered_exactly_once_across_the_surface() {
    // What a list of answers buys back from the compiler: the builders walk
    // `Predicate::PLAN_LEVEL` and `Predicate::PER_KEY`, so a seventh predicate
    // added to `ALL` and to neither roster fails here rather than being silently
    // unanswered.
    let surface = surface_of(sellable_facts());

    let mut answered: Vec<u8> = surface
        .plan_answers
        .iter()
        .chain(surface.keys.iter().flat_map(|key| key.answers.iter()))
        .map(|outcome| outcome.predicate.ordinal())
        .collect();
    answered.sort_unstable();

    assert_eq!(answered, vec![1, 2, 3, 4, 5, 6]);
    // **The count this module's prose states, pinned against the roster.** The
    // module doc says "four of six" and "all six predicates" in several places, and
    // `inst-sg-eligibility-gated` designs a **seventh** — the first payer-dependent
    // one, F-88-gated. So the day that lands, every one of those sentences becomes
    // false; this is what makes the wave that adds it meet them, rather than leaving
    // a count beside a roster where only one of the two stayed true.
    assert_eq!(Predicate::ALL.len(), 6);
    for predicate in Predicate::ALL {
        assert_ne!(
            Predicate::PER_KEY.contains(predicate),
            Predicate::PLAN_LEVEL.contains(predicate),
            "predicate ({}) must be in exactly one roster",
            predicate.ordinal()
        );
    }
}

#[test]
fn the_ordinals_are_the_design_sets_own_numbering() {
    // The design set refers to these predicates by number in every decision that
    // touches them, so the numbers are part of what a reader matches on and are
    // pinned against literals rather than derived from the list's order.
    assert_eq!(Predicate::ActiveWindowWithHorizon.ordinal(), 1);
    assert_eq!(Predicate::CommittedVersion.ordinal(), 2);
    assert_eq!(Predicate::AvailabilityDates.ordinal(), 3);
    assert_eq!(Predicate::PlanLifecycleState.ordinal(), 4);
    assert_eq!(Predicate::GaGateFlags.ordinal(), 5);
    assert_eq!(Predicate::RegistrySellable.ordinal(), 6);
}

#[test]
fn the_surface_carries_the_key_intervals_and_a_derived_coverage_end() {
    // What `inst-sg-surface` requires a consumer be given: the intervals and states
    // as the version froze them, and the derived coverage end — so a consumer can
    // re-evaluate the horizon at another `t` without a second call.
    let facts = PinnedFacts {
        windows: vec![windows_of(
            recurring(),
            vec![
                WindowInterval::new(at(0), Some(at(5)), WindowState::Expired),
                WindowInterval::new(at(5), Some(at(60)), WindowState::Active),
            ],
        )],
        ..sellable_facts()
    };

    let key = &surface_of(facts).keys[0];

    assert_eq!(
        key.intervals,
        vec![
            WindowInterval::new(at(0), Some(at(5)), WindowState::Expired),
            WindowInterval::new(at(5), Some(at(60)), WindowState::Active),
        ],
        "the frozen intervals reach the consumer, states included"
    );
    assert_eq!(
        key.coverage_end,
        CoverageEnd::Ends(at(60)),
        "the coverage end is derived over the whole run, the expired predecessor included"
    );
}

#[test]
fn a_past_instant_inside_an_expired_interval_still_answers_covered() {
    // Why predicate (1)'s containment does not filter on `COVERING_STATES` the way
    // `KeyCoverage::covers` does. This is the one input the two readings disagree
    // on: an `expired` interval and an `at` inside it, i.e. a question about the
    // past. A frozen version has to answer a past order instant the same way
    // forever, so folding the state in would make the answer depend on when it was
    // asked.
    let facts = PinnedFacts {
        available_from: None,
        windows: vec![windows_of(
            recurring(),
            vec![
                WindowInterval::new(at(-40), Some(at(-5)), WindowState::Expired),
                WindowInterval::new(at(-5), None, WindowState::Active),
            ],
        )],
        ..sellable_facts()
    };

    let surface =
        SellabilitySurface::of_delta(&SellabilityFacts::Pinned(facts), at(-20), &eur(), &eu());

    assert_eq!(
        key_answer(&surface.keys[0], Predicate::ActiveWindowWithHorizon),
        &PredicateAnswer::Satisfied,
        "the interval that was effective at that instant is what answers"
    );
}

#[test]
fn an_expired_only_key_answers_a_past_instant_off_both_halves() {
    // The **second** class the literal and the derived readings of predicate (1)
    // part on, staged with no `active` token anywhere in the world: a key whose one
    // interval has since expired, asked about an instant inside it.
    //
    // The case above stages an active successor, so its *horizon* half is answered
    // by that successor and only its containment half is on the expired interval.
    // Here both halves are — `covers_at` admits the interval, and `coverage_end`
    // folds it in where §3 words the horizon over **active-plus-scheduled**
    // coverage. That is the second half of the divergence the module doc records,
    // and it is asserted rather than left to inspection.
    //
    // The market sells no recurring row, so W6's margin is zero and the horizon
    // reduces to "a window covers `t`" — what is asserted is the containment plus a
    // coverage end that exists at all.
    let facts = PinnedFacts {
        available_from: None,
        price_keys: vec![usage()],
        windows: vec![windows_of(
            usage(),
            vec![WindowInterval::new(
                at(-40),
                Some(at(-5)),
                WindowState::Expired,
            )],
        )],
        ..sellable_facts()
    };

    let surface =
        SellabilitySurface::of_delta(&SellabilityFacts::Pinned(facts), at(-20), &eur(), &eu());

    let key = &surface.keys[0];
    assert_eq!(
        key.intervals
            .iter()
            .map(|interval| interval.state)
            .collect::<Vec<_>>(),
        vec![WindowState::Expired],
        "the literal reading has no active window to find"
    );
    assert_eq!(
        key.coverage_end,
        CoverageEnd::Ends(at(-5)),
        "the horizon half folds the expired interval in; the active-plus-scheduled \
         wording read literally would answer Uncovered here"
    );
    assert_eq!(
        key_answer(key, Predicate::ActiveWindowWithHorizon),
        &PredicateAnswer::Satisfied,
        "a frozen version answers a past order instant off the interval that was \
         effective then"
    );
}

#[test]
fn the_surface_takes_no_payer_and_holds_no_cache() {
    // `inst-sg-segment-boundary` — all six predicates are payer-independent — and
    // `inst-sg-eligibility-gated`'s one obligation on this phase: no global
    // sellability cache keyed by plan alone, because the seventh predicate is the
    // first payer-dependent one.
    //
    // **This is a type-and-absence guard and cannot be anything else.** There is no
    // cache whose behaviour a test could drive; what can be asserted is that the
    // constructor takes a delta, an instant and a market — a payer parameter or a
    // cache handle would not compile past this call — and that the same inputs are
    // re-evaluated rather than looked up, while a different instant answers
    // differently off the *same* delta.
    let facts = SellabilityFacts::Pinned(sellable_facts());

    let first = SellabilitySurface::of_delta(&facts, at(10), &eur(), &eu());
    let second = SellabilitySurface::of_delta(&facts, at(10), &eur(), &eu());
    assert_eq!(first, second, "the surface is a function of its arguments");

    let earlier = SellabilitySurface::of_delta(&facts, at(-1), &eur(), &eu());
    assert!(matches!(
        answer(&earlier, Predicate::AvailabilityDates),
        PredicateAnswer::Failed { .. }
    ));
}

#[test]
fn every_answer_is_one_of_the_three_tokens() {
    // **A vocabulary check, and named for that.** It was called
    // `nothing_the_surface_carries_is_a_point_in_time_boolean` and did not check
    // that: it reads `as_str` off each answer and off the verdict, which says what
    // the tokens *are*, not that no field is a `bool`. That property is a type
    // property — the compiler is its guard and the module doc says so, per the
    // crate's "where a guard is a type, say so" rule — and the wire half of it is
    // the real assertion, over the rendered document in `tests/rest_windows.rs`.
    //
    // What this does hold is worth holding on its own: a fourth answer arm, or a
    // renamed token, reaches a consumer as an unrecognised string, and the three
    // literals here are the ones D-167 clause (3) makes a consumer branch on.
    let surface = surface_of(sellable_facts());

    for outcome in surface
        .plan_answers
        .iter()
        .chain(surface.keys.iter().flat_map(|key| key.answers.iter()))
    {
        let PredicateOutcome { predicate, answer } = outcome;
        assert!(
            ["satisfied", "failed", "not_evaluable"].contains(&answer.as_str()),
            "predicate ({}) answered outside the three-token vocabulary",
            predicate.ordinal()
        );
    }
    assert!(
        ["sellable", "not_sellable", "not_evaluable"]
            .contains(&surface.plan_market_verdict().as_str())
    );
}

#[test]
fn the_gates_facts_carry_no_bundle_operand() {
    // `inst-sg-bundle` asks the surface to "expose the frozen component key set"
    // and walk it. **This is the assertion that the operand for that walk is not
    // here**, and it is an exhaustive destructure rather than a member count
    // because a count cannot say *which* member arrived: the day `PinnedFacts`
    // grows a bundle discriminator, a `PriceBasis` or a component set, this stops
    // compiling and the reader is sent to `crate::domain::bundle_sellability`,
    // which holds the rule and has waited for exactly this.
    //
    // The destructure is over `PinnedFacts` and not over the payload: the payload
    // roster is already pinned by `read_model_repo_tests`'
    // `the_payloads_members_partition_into_the_read_and_the_ignored`, and these
    // are the two ends of one absence — nothing projects a bundle fact, and
    // nothing here could read one if it did.
    let PinnedFacts {
        plan_id,
        catalog_version,
        lifecycle_state,
        available_from,
        available_to,
        frequency,
        price_keys,
        windows,
    } = sellable_facts();

    // Named so the destructure is a roster and not eight `let _`s: every member
    // below is a fact about **this** plan-subject, and not one of them can name
    // another plan — which is what a component reference is.
    let _ = (plan_id, catalog_version, lifecycle_state);
    let _ = (available_from, available_to, frequency);
    let _ = (price_keys, windows);
}
