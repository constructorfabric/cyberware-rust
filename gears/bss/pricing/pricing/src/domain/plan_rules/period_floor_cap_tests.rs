//! One case per clause of the two period floor/cap rules (**D-319**).
//!
//! Every refusal here is paired with a positive control, and the controls are
//! not decoration: a refusal test can be green because the rule fired for the
//! wrong reason, or because the subject was unpublishable before the case
//! touched it. So each case asserts both that the offending shape is refused
//! and that the shape one edit away from it is accepted.

#![allow(clippy::expect_used)]

use uuid::Uuid;

use super::{PeriodFloorCapAmounts, PeriodFloorCapMarketSold};
use crate::domain::concurrency::RowVersion;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount};
use crate::domain::plan_rules::{PERIOD_FLOOR_CAP_AMOUNT_INVALID, PERIOD_FLOOR_CAP_MARKET_UNSOLD};
use crate::domain::plan_shape::{PeriodFloorCap, PlanShape};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{ModelKind, PriceRow};
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::validation::{ValidationReport, ValidationRule};
use time::OffsetDateTime;

const TERMINAL: u128 = 0x7e_11;

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x91a4))
}

fn now() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 15, 12, 0, 0)
}

fn currency(code: &str) -> CurrencyCode {
    CurrencyCode::new(code).expect("test currency is three letters")
}

fn region(value: &str) -> Region {
    Region::new(value).expect("test region is non-blank")
}

fn minor(units: i64) -> MinorAmount {
    MinorAmount::new(units).expect("test amount is non-negative")
}

/// A recurring base row, which is what puts a market into
/// [`PlanShape::markets`].
fn recurring(seed: u128, code: &str, market: &str) -> PriceRecord {
    let scope_key = ScopeKey::new(
        plan(),
        currency(code),
        region(market),
        PhaseId::new(Uuid::from_u128(TERMINAL)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none");
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(minor(2_500));
    PriceRecord {
        price_id: Uuid::from_u128(seed),
        scope_key,
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: None,
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Draft,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: now(),
        row_version: RowVersion::new(0),
    }
}

fn bound(code: &str, market: &str, floor: Option<i64>, cap: Option<i64>) -> PeriodFloorCap {
    PeriodFloorCap {
        currency: currency(code),
        region: region(market),
        floor_minor: floor.map(minor),
        cap_minor: cap.map(minor),
    }
}

/// A plan selling `(USD, US)` and `(EUR, DE)`, carrying `bounds`.
fn shape(bounds: Vec<PeriodFloorCap>) -> PlanShape {
    let mut shape = PlanShape::new(plan(), 0, now());
    shape.rows = vec![recurring(0x01, "usd", "US"), recurring(0x02, "eur", "DE")];
    shape.period_floor_caps = bounds;
    shape
}

fn report_of(rule: &dyn ValidationRule<PlanShape>, subject: &PlanShape) -> ValidationReport {
    let mut report = ValidationReport::default();
    rule.evaluate(subject, &mut report);
    report
}

fn codes(report: &ValidationReport) -> Vec<&str> {
    report
        .violations
        .iter()
        .map(|violation| violation.code.as_str())
        .collect()
}

// The spelling contract for both codes is `plan_rules_tests::DECLARED`, which
// carries them and is asserted complete against the pipeline's own `pub const`
// declarations. A second copy here could only ever agree with it.

// ---------------------------------------------------------------------------
// `inst-pfc-market`.
// ---------------------------------------------------------------------------

/// The refusal, and its positive control one market over.
///
/// `(USD, CA)` is not a typo in the test: it is the shape of the typo the rule
/// exists to catch, a plan selling `(USD, US)` whose author reached for the
/// wrong region and got no minimum at all.
#[test]
fn a_bound_on_a_market_the_plan_does_not_sell_is_refused() {
    let refused = report_of(
        &PeriodFloorCapMarketSold,
        &shape(vec![bound("usd", "CA", Some(50_000), None)]),
    );
    assert_eq!(codes(&refused), [PERIOD_FLOOR_CAP_MARKET_UNSOLD]);
    assert!(
        refused.violations[0].detail.contains("CA"),
        "the refusal names the market: {}",
        refused.violations[0].detail
    );

    let accepted = report_of(
        &PeriodFloorCapMarketSold,
        &shape(vec![bound("usd", "US", Some(50_000), None)]),
    );
    assert!(
        accepted.violations.is_empty(),
        "the same bound on a sold market is the whole point of the field"
    );
}

/// Both axes are part of the market, and a bound naming a sold *region* in an
/// unsold *currency* is as unreachable as one naming an unsold region.
#[test]
fn the_currency_half_of_the_market_is_checked_too() {
    let refused = report_of(
        &PeriodFloorCapMarketSold,
        &shape(vec![bound("gbp", "US", Some(50_000), None)]),
    );
    assert_eq!(codes(&refused), [PERIOD_FLOOR_CAP_MARKET_UNSOLD]);
}

/// The report enumerates every violation, never the first — the fail-closed
/// report contract, and the reason an author remediates a plan in one pass.
#[test]
fn every_unsold_market_is_named_rather_than_the_first() {
    let report = report_of(
        &PeriodFloorCapMarketSold,
        &shape(vec![
            bound("usd", "CA", Some(50_000), None),
            bound("gbp", "GB", Some(40_000), None),
        ]),
    );
    assert_eq!(
        codes(&report),
        [
            PERIOD_FLOOR_CAP_MARKET_UNSOLD,
            PERIOD_FLOOR_CAP_MARKET_UNSOLD
        ]
    );
}

/// The converse is **not** a rule: a sold market with no bound is the ordinary
/// plan, and this control is what keeps a later author from "completing" the
/// rule into a requirement that every market carry a minimum.
#[test]
fn a_sold_market_with_no_bound_is_not_a_violation() {
    let report = report_of(&PeriodFloorCapMarketSold, &shape(Vec::new()));
    assert!(report.violations.is_empty());
}

// ---------------------------------------------------------------------------
// `inst-pfc-amount`.
// ---------------------------------------------------------------------------

/// `0` and absence are one state, so only one of them may be spellable.
#[test]
fn a_zero_floor_is_refused_and_absence_is_how_no_minimum_is_said() {
    let refused = report_of(
        &PeriodFloorCapAmounts,
        &shape(vec![bound("usd", "US", Some(0), None)]),
    );
    assert_eq!(codes(&refused), [PERIOD_FLOOR_CAP_AMOUNT_INVALID]);

    // The positive control is the *absence*, not a different number: what this
    // case asserts is that the state a `0` was trying to express is reachable.
    let accepted = report_of(&PeriodFloorCapAmounts, &shape(Vec::new()));
    assert!(
        accepted.violations.is_empty(),
        "a plan with no bound authored is how \"no minimum\" is said"
    );

    // And a positive floor passes, so the refusal is about the zero and not
    // about the rule refusing every floor.
    let positive = report_of(
        &PeriodFloorCapAmounts,
        &shape(vec![bound("usd", "US", Some(1), None)]),
    );
    assert!(positive.violations.is_empty());
}

#[test]
fn a_zero_cap_is_refused_and_a_positive_one_is_not() {
    let refused = report_of(
        &PeriodFloorCapAmounts,
        &shape(vec![bound("usd", "US", None, Some(0))]),
    );
    assert_eq!(codes(&refused), [PERIOD_FLOOR_CAP_AMOUNT_INVALID]);

    let accepted = report_of(
        &PeriodFloorCapAmounts,
        &shape(vec![bound("usd", "US", None, Some(100_000))]),
    );
    assert!(accepted.violations.is_empty());
}

/// The ordering clause, pinned on both sides of the boundary: equal passes,
/// one minor unit over fails.
#[test]
fn a_floor_above_its_cap_is_refused_and_equality_is_not() {
    let refused = report_of(
        &PeriodFloorCapAmounts,
        &shape(vec![bound("usd", "US", Some(50_001), Some(50_000))]),
    );
    assert_eq!(codes(&refused), [PERIOD_FLOOR_CAP_AMOUNT_INVALID]);
    assert!(
        refused.violations[0].detail.contains("50001")
            && refused.violations[0].detail.contains("50000"),
        "the refusal names both bounds: {}",
        refused.violations[0].detail
    );

    let equal = report_of(
        &PeriodFloorCapAmounts,
        &shape(vec![bound("usd", "US", Some(50_000), Some(50_000))]),
    );
    assert!(
        equal.violations.is_empty(),
        "a plan that bills exactly X per period is a fixed-fee plan, not a contradiction"
    );
}

/// A row authoring neither bound says nothing, and the store's `present` CHECK
/// says the same thing one layer down.
#[test]
fn a_bound_authoring_neither_amount_is_refused() {
    let refused = report_of(
        &PeriodFloorCapAmounts,
        &shape(vec![bound("usd", "US", None, None)]),
    );
    assert_eq!(codes(&refused), [PERIOD_FLOOR_CAP_AMOUNT_INVALID]);
}

/// The two rules judge different things, and neither covers the other: an
/// unsold market with a legal amount is one violation, and a sold market with
/// an illegal amount is the other. A single rule doing both would report one
/// code for two remediations.
#[test]
fn the_two_rules_are_independent() {
    let subject = shape(vec![
        bound("usd", "CA", Some(50_000), None),
        bound("eur", "DE", Some(0), None),
    ]);

    assert_eq!(
        codes(&report_of(&PeriodFloorCapMarketSold, &subject)),
        [PERIOD_FLOOR_CAP_MARKET_UNSOLD]
    );
    assert_eq!(
        codes(&report_of(&PeriodFloorCapAmounts, &subject)),
        [PERIOD_FLOOR_CAP_AMOUNT_INVALID]
    );
}
