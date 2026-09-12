//! `CurrencyBindingChecker` case (i) — `design/04-currency-tax.md` §3
//! `inst-cb-addon`, D-95, C-3.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use uuid::Uuid;

use super::{
    AddonCoverage, CURRENCY_NOT_COVERED, Market, RequiredAddonsCoverMarkets, sold_markets,
};
use crate::domain::concurrency::RowVersion;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount};
use crate::domain::plan_shape::{AddonRule, PlanShape};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::PriceRow;
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::validation::{ValidationReport, ValidationRule};
use time::OffsetDateTime;

/// One add-on and the markets a fixture gives it.
type CoverageSpec<'a> = (Uuid, &'a [(&'a str, &'a str)]);

const ADDON_A: Uuid = Uuid::from_u128(0x0add_000a);
const ADDON_B: Uuid = Uuid::from_u128(0x0add_000b);

fn now() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 6, 12, 0, 0)
}

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x91a4))
}

fn market(currency: &str, region: &str) -> Market {
    (
        CurrencyCode::new(currency).expect("three letters"),
        Region::new(region).expect("non-blank"),
    )
}

fn row(price_id: u128, currency: &str, region: &str, eligibility: PriceEligibility) -> PriceRecord {
    let cohort = if eligibility == PriceEligibility::ExistingGrandfathered {
        Cohort::Generation(now())
    } else {
        Cohort::None
    };
    let scope_key = ScopeKey::new(
        plan(),
        CurrencyCode::new(currency).expect("three letters"),
        Region::new(region).expect("non-blank"),
        PhaseId::new(Uuid::from_u128(0xf1)),
        eligibility,
        ChargeKind::Recurring,
        cohort,
    )
    .expect("the eligibility and cohort pair");

    let mut shape = PriceRow::new(ChargeKind::Recurring, None);
    shape.amount_minor = Some(MinorAmount::new(1000).expect("non-negative"));

    PriceRecord {
        price_id: Uuid::from_u128(price_id),
        scope_key,
        row: shape,
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

fn addon(sku: Uuid, required: bool, depends_on: Vec<Uuid>) -> AddonRule {
    AddonRule {
        addon_sku_id: sku,
        required,
        min_qty: None,
        max_qty: None,
        step_qty: None,
        price_override_ref: None,
        depends_on,
        conflicts_with: Vec::new(),
    }
}

/// A plan selling the given markets, with the given add-on rules.
fn plan_selling(markets: &[(&str, &str)], addons: Vec<AddonRule>) -> PlanShape {
    let mut shape = PlanShape::new(plan(), 1, now());
    shape.rows = markets
        .iter()
        .enumerate()
        .map(|(n, (c, r))| row(0xb000 + n as u128, c, r, PriceEligibility::AllSubscriptions))
        .collect();
    shape.addon_rules = addons;
    shape
}

fn coverage(entries: &[CoverageSpec]) -> AddonCoverage {
    AddonCoverage::new(
        entries
            .iter()
            .map(|(sku, markets)| {
                (
                    *sku,
                    markets
                        .iter()
                        .map(|(c, r)| market(c, r))
                        .collect::<BTreeSet<Market>>(),
                )
            })
            .collect::<BTreeMap<_, _>>(),
    )
}

fn run(rule: &RequiredAddonsCoverMarkets, shape: &PlanShape) -> ValidationReport {
    let mut report = ValidationReport::default();
    rule.evaluate(shape, &mut report);
    report
}

fn codes(report: &ValidationReport) -> Vec<String> {
    report.violations.iter().map(|v| v.code.clone()).collect()
}

#[test]
fn the_code_is_spelled_as_section_5_spells_it() {
    assert_eq!(CURRENCY_NOT_COVERED, "CURRENCY_NOT_COVERED");
}

#[test]
fn the_rule_names_the_instruction_it_implements() {
    assert_eq!(
        RequiredAddonsCoverMarkets::default().name(),
        "inst-cb-addon"
    );
}

// ---------------------------------------------------------------------------
// The positive control.
// ---------------------------------------------------------------------------

/// A required add-on covering every sold market publishes.
#[test]
fn a_required_addon_covering_every_market_passes() {
    let shape = plan_selling(
        &[("EUR", "eu"), ("USD", "us")],
        vec![addon(ADDON_A, true, vec![])],
    );

    let report = run(
        &RequiredAddonsCoverMarkets {
            coverage: coverage(&[(ADDON_A, &[("EUR", "eu"), ("USD", "us")])]),
        },
        &shape,
    );

    assert_eq!(codes(&report), Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// D-95 — the pair, not the currency.
// ---------------------------------------------------------------------------

/// A required add-on missing a sold market is refused, naming both.
#[test]
fn a_required_addon_missing_a_market_is_refused_naming_it() {
    let shape = plan_selling(
        &[("EUR", "eu"), ("USD", "us")],
        vec![addon(ADDON_A, true, vec![])],
    );

    let report = run(
        &RequiredAddonsCoverMarkets {
            coverage: coverage(&[(ADDON_A, &[("EUR", "eu")])]),
        },
        &shape,
    );

    assert_eq!(codes(&report), [CURRENCY_NOT_COVERED]);
    assert_eq!(report.violations[0].subject, ADDON_A.to_string());
    assert!(
        report.violations[0].detail.contains("USD/us"),
        "the missing market is named: {}",
        report.violations[0].detail
    );
}

/// **D-95's own worked example.** The add-on covers the currency, but in the
/// wrong region.
///
/// Under the currency-only reading this passed publish and died at order
/// assembly. It is the case the whole pair-wise domain exists for, so it is
/// asserted separately from the plain missing-market case above — which a
/// currency-only implementation would still catch.
#[test]
fn covering_the_currency_in_the_wrong_region_is_not_coverage() {
    let shape = plan_selling(&[("EUR", "eu")], vec![addon(ADDON_A, true, vec![])]);

    let report = run(
        &RequiredAddonsCoverMarkets {
            coverage: coverage(&[(ADDON_A, &[("EUR", "us")])]),
        },
        &shape,
    );

    assert_eq!(
        codes(&report),
        [CURRENCY_NOT_COVERED],
        "EUR in `us` does not cover EUR in `eu` (D-95)"
    );
}

// ---------------------------------------------------------------------------
// The optional carve-out.
// ---------------------------------------------------------------------------

/// An **unreferenced** optional add-on's gap does not block the base publish.
///
/// It is enforced at attachment instead. Without this carve-out a tenant could
/// not publish a plan carrying a promotional add-on that covers two of its
/// twenty markets.
#[test]
fn an_unreferenced_optional_addon_with_a_gap_still_publishes() {
    let shape = plan_selling(
        &[("EUR", "eu"), ("USD", "us")],
        vec![addon(ADDON_A, false, vec![])],
    );

    let report = run(
        &RequiredAddonsCoverMarkets {
            coverage: coverage(&[(ADDON_A, &[("EUR", "eu")])]),
        },
        &shape,
    );

    assert_eq!(codes(&report), Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// C-3 — the dependency closure.
// ---------------------------------------------------------------------------

/// **C-3's case.** An *optional* add-on a required one `depends_on` is
/// transitively mandatory, and its gap blocks.
///
/// Under the flat required-set reading its coverage was never checked — the same
/// D-95 asymmetry arriving through the dependency door, ending at the same
/// order-assembly failure on a plan that published clean.
#[test]
fn an_optional_addon_a_required_one_depends_on_must_cover_too() {
    let shape = plan_selling(
        &[("EUR", "eu"), ("USD", "us")],
        vec![
            addon(ADDON_A, true, vec![ADDON_B]),
            addon(ADDON_B, false, vec![]),
        ],
    );

    let report = run(
        &RequiredAddonsCoverMarkets {
            coverage: coverage(&[
                (ADDON_A, &[("EUR", "eu"), ("USD", "us")]),
                (ADDON_B, &[("EUR", "eu")]),
            ]),
        },
        &shape,
    );

    assert_eq!(codes(&report), [CURRENCY_NOT_COVERED]);
    assert_eq!(
        report.violations[0].subject,
        ADDON_B.to_string(),
        "the transitively mandatory add-on is the one at fault"
    );
}

/// A dependency **cycle** terminates rather than hanging.
///
/// Cycles are `ADDON_CYCLE`'s to refuse; this rule must not spin on a plan
/// another rule is about to reject, or a malformed composition becomes a hang
/// instead of a refusal.
#[test]
fn a_dependency_cycle_terminates() {
    let shape = plan_selling(
        &[("EUR", "eu")],
        vec![
            addon(ADDON_A, true, vec![ADDON_B]),
            addon(ADDON_B, false, vec![ADDON_A]),
        ],
    );

    let report = run(
        &RequiredAddonsCoverMarkets {
            coverage: coverage(&[(ADDON_A, &[("EUR", "eu")]), (ADDON_B, &[("EUR", "eu")])]),
        },
        &shape,
    );

    assert_eq!(codes(&report), Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// The sold-market domain.
// ---------------------------------------------------------------------------

/// An add-on the caller could not resolve covers **nothing**, which is the
/// fail-closed reading.
#[test]
fn an_unresolved_addon_covers_nothing() {
    let shape = plan_selling(&[("EUR", "eu")], vec![addon(ADDON_A, true, vec![])]);

    let report = run(
        &RequiredAddonsCoverMarkets {
            coverage: AddonCoverage::default(),
        },
        &shape,
    );

    assert_eq!(codes(&report), [CURRENCY_NOT_COVERED]);
}

/// Grandfathered generations are not markets a plan **sells**.
///
/// ADR-0002, and the narrowing `inst-bc-coverage` states one plane over: a
/// cutover-touched generation is never a coverage candidate. Counting it would
/// demand that every required add-on cover a market the plan reaches only through
/// a frozen generation nobody can subscribe to.
#[test]
fn a_grandfathered_only_market_is_not_a_sold_market() {
    let mut shape = plan_selling(&[("EUR", "eu")], vec![addon(ADDON_A, true, vec![])]);
    shape.rows.push(row(
        0xb0_99,
        "JPY",
        "jp",
        PriceEligibility::ExistingGrandfathered,
    ));

    assert_eq!(
        sold_markets(&shape),
        [market("EUR", "eu")].into_iter().collect::<BTreeSet<_>>()
    );

    let report = run(
        &RequiredAddonsCoverMarkets {
            coverage: coverage(&[(ADDON_A, &[("EUR", "eu")])]),
        },
        &shape,
    );
    assert_eq!(codes(&report), Vec::<String>::new());
}

/// **A `new_subscriptions_only` market is sold, and a required add-on must cover
/// it.**
///
/// `sold_markets` filters on `!= ExistingGrandfathered`, so the eligibility axis
/// has three values and two of them are on the sold side. Only `all_subscriptions`
/// and `existing_grandfathered` had a fixture, and a filter narrowed to
/// `== AllSubscriptions` - the shape a reader could take the rule for - is
/// refuted by nothing without this case: it would drop a live market out of the
/// coverage walk and let a required add-on publish uncovered on it.
#[test]
fn a_new_subscriptions_only_market_is_sold_and_must_be_covered() {
    let mut shape = plan_selling(&[("EUR", "eu")], vec![addon(ADDON_A, true, vec![])]);
    shape.rows.push(row(
        0xb0_98,
        "USD",
        "us",
        PriceEligibility::NewSubscriptionsOnly,
    ));

    assert_eq!(
        sold_markets(&shape),
        [market("EUR", "eu"), market("USD", "us")]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        "a market open to new subscribers is a market the plan sells"
    );

    // Covered on `EUR/eu` alone, so the required add-on is short of the market the
    // new-subscriptions row opened - and the rule has to say so.
    let report = run(
        &RequiredAddonsCoverMarkets {
            coverage: coverage(&[(ADDON_A, &[("EUR", "eu")])]),
        },
        &shape,
    );
    assert_eq!(codes(&report), vec![CURRENCY_NOT_COVERED]);

    // And it publishes once the add-on reaches it: the refusal above is about the
    // gap and not about the eligibility.
    let covered = run(
        &RequiredAddonsCoverMarkets {
            coverage: coverage(&[(ADDON_A, &[("EUR", "eu"), ("USD", "us")])]),
        },
        &shape,
    );
    assert_eq!(codes(&covered), Vec::<String>::new());
}

/// A plan with no candidate rows sells nothing, so nothing has to be covered.
#[test]
fn a_plan_selling_nothing_demands_no_coverage() {
    let shape = plan_selling(&[], vec![addon(ADDON_A, true, vec![])]);

    let report = run(
        &RequiredAddonsCoverMarkets {
            coverage: AddonCoverage::default(),
        },
        &shape,
    );

    assert_eq!(codes(&report), Vec::<String>::new());
}
