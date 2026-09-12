//! `TaxDisplayValidator`'s rules — `design/04-currency-tax.md` §3 step 2/3a, C3,
//! C4, D-110, D-132, D-154.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{
    MarketBasisUniform, RegionReadiness, RegionTaxReadiness, TAX_BASIS_INCOMPLETE,
    TAX_BASIS_MIXED_MARKET, TaxBasisComplete, TaxDisplayPolicy, effective_category,
    is_not_sellable_ga,
};
use crate::domain::concurrency::RowVersion;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount};
use crate::domain::plan_shape::PlanShape;
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::PriceRow;
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::validation::{ValidationReport, ValidationRule};
use time::OffsetDateTime;

fn now() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 6, 12, 0, 0)
}

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x91a4))
}

/// One candidate row: market, basis, own category, eligibility.
fn row(
    price_id: u128,
    currency: &str,
    region: &str,
    tax_inclusive: bool,
    category: Option<&str>,
    eligibility: PriceEligibility,
) -> PriceRecord {
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
        tax_inclusive,
        tax_category_ref: category.map(ToOwned::to_owned),
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

fn shape_of(rows: Vec<PriceRecord>) -> PlanShape {
    let mut shape = PlanShape::new(plan(), 1, now());
    shape.rows = rows;
    shape
}

/// A readiness lookup declaring one region.
fn readiness(region: &str, category: Option<&str>, rate_present: bool) -> RegionTaxReadiness {
    RegionTaxReadiness::new(
        [(
            region.to_owned(),
            RegionReadiness {
                tax_category: category.map(ToOwned::to_owned),
                tax_rate_present: rate_present,
            },
        )]
        .into_iter()
        .collect(),
    )
}

fn run(rule: &impl ValidationRule<PlanShape>, shape: &PlanShape) -> ValidationReport {
    let mut report = ValidationReport::default();
    rule.evaluate(shape, &mut report);
    report
}

fn codes(report: &ValidationReport) -> Vec<String> {
    report.violations.iter().map(|v| v.code.clone()).collect()
}

fn warnings(report: &ValidationReport) -> Vec<String> {
    report.warnings.iter().map(|w| w.code.clone()).collect()
}

fn policy(mode: TaxDisplayPolicy, readiness: RegionTaxReadiness) -> TaxBasisComplete {
    TaxBasisComplete {
        policy: mode,
        readiness,
    }
}

// ---------------------------------------------------------------------------
// The codes, transcribed from section 5.
// ---------------------------------------------------------------------------

#[test]
fn the_codes_are_spelled_as_section_5_spells_them() {
    assert_eq!(TAX_BASIS_INCOMPLETE, "TAX_BASIS_INCOMPLETE");
    assert_eq!(TAX_BASIS_MIXED_MARKET, "TAX_BASIS_MIXED_MARKET");
}

#[test]
fn the_policy_tokens_round_trip_and_default_to_fail_closed() {
    for &mode in TaxDisplayPolicy::ALL {
        assert_eq!(TaxDisplayPolicy::parse(mode.as_str()), Some(mode));
    }
    assert_eq!(TaxDisplayPolicy::parse("permissive"), None);
    assert_eq!(
        TaxDisplayPolicy::default(),
        TaxDisplayPolicy::FailClosed,
        "C4 is fail-closed for **all** tenants"
    );
}

// ---------------------------------------------------------------------------
// D-154's coalesce.
// ---------------------------------------------------------------------------

/// The row's own category wins over the region default.
#[test]
fn the_rows_own_category_is_the_effective_one() {
    let record = row(
        0xb1,
        "EUR",
        "eu",
        false,
        Some("reduced"),
        PriceEligibility::AllSubscriptions,
    );

    let resolved = effective_category(&record, &readiness("eu", Some("standard"), true));

    assert_eq!(resolved.as_deref(), Some("reduced"));
}

/// A row stating none falls back to the region's default.
#[test]
fn a_row_stating_no_category_falls_back_to_the_regions_default() {
    let record = row(
        0xb1,
        "EUR",
        "eu",
        false,
        None,
        PriceEligibility::AllSubscriptions,
    );

    let resolved = effective_category(&record, &readiness("eu", Some("standard"), true));

    assert_eq!(resolved.as_deref(), Some("standard"));
}

/// **The design set's own worked example**: a row-level category satisfies the
/// check in a region whose taxonomy carries no default.
///
/// §3 step 2 names this case explicitly (2026-07-28 review fix, confirmed
/// 2026-07-31). It is the case that would become unreachable if an undeclared
/// region and a declared-but-categoryless one were collapsed into one answer.
#[test]
fn a_row_level_category_satisfies_a_region_with_no_default() {
    let record = row(
        0xb1,
        "EUR",
        "eu",
        false,
        Some("standard"),
        PriceEligibility::AllSubscriptions,
    );
    let shape = shape_of(vec![record]);

    let report = run(
        &policy(TaxDisplayPolicy::FailClosed, readiness("eu", None, true)),
        &shape,
    );

    assert_eq!(codes(&report), Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// `inst-td-policy` — the category arm (unconditional, D-154).
// ---------------------------------------------------------------------------

/// No row category and no region default is `TAX_BASIS_INCOMPLETE`.
#[test]
fn an_absent_effective_category_blocks_publish() {
    let shape = shape_of(vec![row(
        0xb1,
        "EUR",
        "eu",
        false,
        None,
        PriceEligibility::AllSubscriptions,
    )]);

    let report = run(
        &policy(TaxDisplayPolicy::FailClosed, readiness("eu", None, false)),
        &shape,
    );

    assert_eq!(codes(&report), [TAX_BASIS_INCOMPLETE]);
}

/// **The half of D-154 that is easiest to get wrong.** The category arm has **no
/// warn mode**: it blocks even for a tenant who explicitly selected `warn`.
///
/// `taxCategory` is a pinned D-48 v1 descriptor element whose absence S2
/// `inst-ds-required` and PRD `fr-billing-descriptors` both make a
/// publish-blocking MUST, and a per-tenant display policy may not publish past a
/// pinned contract element. Read as one switch with the rate arm, a tenant on
/// `warn` would publish a row whose category no consumer can resolve.
#[test]
fn the_category_arm_blocks_even_under_the_warn_policy() {
    let shape = shape_of(vec![row(
        0xb1,
        "EUR",
        "eu",
        false,
        None,
        PriceEligibility::AllSubscriptions,
    )]);

    let report = run(
        &policy(TaxDisplayPolicy::Warn, readiness("eu", None, false)),
        &shape,
    );

    assert_eq!(
        codes(&report),
        [TAX_BASIS_INCOMPLETE],
        "a display policy may not publish past a pinned descriptor element (D-154)"
    );
    assert!(
        !report.is_publishable(),
        "and it is a violation, not an advisory"
    );
}

// ---------------------------------------------------------------------------
// `inst-td-policy` — the rate arm (policy-governed).
// ---------------------------------------------------------------------------

/// A tax-inclusive row in a region with no declared rate blocks under the
/// default fail-closed policy — and C4 precedes C3's GA flag.
#[test]
fn a_tax_inclusive_row_without_a_declared_rate_blocks_under_fail_closed() {
    let shape = shape_of(vec![row(
        0xb1,
        "EUR",
        "eu",
        true,
        Some("standard"),
        PriceEligibility::AllSubscriptions,
    )]);

    let report = run(
        &policy(TaxDisplayPolicy::FailClosed, readiness("eu", None, false)),
        &shape,
    );

    assert_eq!(codes(&report), [TAX_BASIS_INCOMPLETE]);
}

/// The same row **warns** rather than blocking under an explicit `warn` policy.
///
/// This is the arm the policy still governs: the missing fact is a *rate*,
/// nobody in this gear owns it, and Tax Engine is pre-GA.
#[test]
fn the_rate_arm_warns_rather_than_blocking_under_the_warn_policy() {
    let shape = shape_of(vec![row(
        0xb1,
        "EUR",
        "eu",
        true,
        Some("standard"),
        PriceEligibility::AllSubscriptions,
    )]);

    let report = run(
        &policy(TaxDisplayPolicy::Warn, readiness("eu", None, false)),
        &shape,
    );

    assert_eq!(codes(&report), Vec::<String>::new());
    assert_eq!(warnings(&report), [TAX_BASIS_INCOMPLETE]);
    assert!(report.is_publishable(), "an advisory never blocks");
}

/// A tax-inclusive row in a region that *does* declare a rate passes.
#[test]
fn a_tax_inclusive_row_with_a_declared_rate_passes() {
    let shape = shape_of(vec![row(
        0xb1,
        "EUR",
        "eu",
        true,
        Some("standard"),
        PriceEligibility::AllSubscriptions,
    )]);

    let report = run(
        &policy(
            TaxDisplayPolicy::FailClosed,
            readiness("eu", Some("standard"), true),
        ),
        &shape,
    );

    assert_eq!(codes(&report), Vec::<String>::new());
    assert_eq!(warnings(&report), Vec::<String>::new());
}

/// An **undeclared** region fails closed on the rate arm too (C4: "an unknown
/// region fails closed").
#[test]
fn an_undeclared_region_fails_closed_on_a_tax_inclusive_row() {
    let shape = shape_of(vec![row(
        0xb1,
        "EUR",
        "mars",
        true,
        Some("standard"),
        PriceEligibility::AllSubscriptions,
    )]);

    let report = run(
        &policy(
            TaxDisplayPolicy::FailClosed,
            readiness("eu", Some("standard"), true),
        ),
        &shape,
    );

    assert_eq!(codes(&report), [TAX_BASIS_INCOMPLETE]);
}

/// A **tax-exclusive** row is not asked about a rate at all.
///
/// The rate arm's condition is `taxInclusive = true`; without that guard every
/// tax-exclusive row in a rate-less region would block, which is MVP's entire
/// sell mode.
#[test]
fn a_tax_exclusive_row_is_not_asked_about_a_rate() {
    let shape = shape_of(vec![row(
        0xb1,
        "EUR",
        "eu",
        false,
        Some("standard"),
        PriceEligibility::AllSubscriptions,
    )]);

    let report = run(
        &policy(TaxDisplayPolicy::FailClosed, readiness("eu", None, false)),
        &shape,
    );

    assert_eq!(
        codes(&report),
        Vec::<String>::new(),
        "MVP sells tax-exclusive; a rate is not its business"
    );
}

// ---------------------------------------------------------------------------
// `inst-td-gagate` (C3) — per row, hence per market.
// ---------------------------------------------------------------------------

/// A tax-inclusive row is gated pre-GA; a tax-exclusive one never is.
#[test]
fn the_ga_gate_is_per_row_and_only_on_tax_inclusive_rows() {
    let inclusive = row(
        0xb1,
        "EUR",
        "eu",
        true,
        Some("standard"),
        PriceEligibility::AllSubscriptions,
    );
    let exclusive = row(
        0xb2,
        "USD",
        "us",
        false,
        Some("standard"),
        PriceEligibility::AllSubscriptions,
    );

    assert!(is_not_sellable_ga(&inclusive, false), "EU is gated");
    assert!(
        !is_not_sellable_ga(&exclusive, false),
        "and the US market stays sellable - the gate is per market, not per plan"
    );
}

/// Post-GA nothing is gated.
#[test]
fn the_ga_gate_clears_once_the_tax_engine_has_ga_ed() {
    let inclusive = row(
        0xb1,
        "EUR",
        "eu",
        true,
        Some("standard"),
        PriceEligibility::AllSubscriptions,
    );

    assert!(!is_not_sellable_ga(&inclusive, true));
}

// ---------------------------------------------------------------------------
// `inst-td-basis-uniform` (D-110, scoped by D-132).
// ---------------------------------------------------------------------------

/// A mixed basis within one market fails, naming both sides.
#[test]
fn a_mixed_basis_in_one_market_fails_naming_the_divergent_rows() {
    let shape = shape_of(vec![
        row(
            0xb1,
            "EUR",
            "eu",
            true,
            Some("s"),
            PriceEligibility::AllSubscriptions,
        ),
        row(
            0xb2,
            "EUR",
            "eu",
            false,
            Some("s"),
            PriceEligibility::AllSubscriptions,
        ),
    ]);

    let report = run(&MarketBasisUniform, &shape);

    assert_eq!(codes(&report), [TAX_BASIS_MIXED_MARKET]);
    let detail = &report.violations[0].detail;
    assert!(
        detail.contains(&Uuid::from_u128(0xb1).to_string()),
        "§5 requires the divergent rows to be named: {detail}"
    );
    assert!(
        detail.contains(&Uuid::from_u128(0xb2).to_string()),
        "{detail}"
    );
}

/// **The rule is per market, not per plan.** Tax-inclusive across all of EU and
/// tax-exclusive across all of US is legal.
#[test]
fn one_basis_per_market_permits_different_bases_in_different_markets() {
    let shape = shape_of(vec![
        row(
            0xb1,
            "EUR",
            "eu",
            true,
            Some("s"),
            PriceEligibility::AllSubscriptions,
        ),
        row(
            0xb2,
            "USD",
            "us",
            false,
            Some("s"),
            PriceEligibility::AllSubscriptions,
        ),
    ]);

    let report = run(&MarketBasisUniform, &shape);

    assert_eq!(codes(&report), Vec::<String>::new());
}

/// The currency is part of the market: one region, two currencies, two markets.
#[test]
fn the_market_is_the_currency_and_the_region_together() {
    let shape = shape_of(vec![
        row(
            0xb1,
            "EUR",
            "eu",
            true,
            Some("s"),
            PriceEligibility::AllSubscriptions,
        ),
        row(
            0xb2,
            "USD",
            "eu",
            false,
            Some("s"),
            PriceEligibility::AllSubscriptions,
        ),
    ]);

    let report = run(&MarketBasisUniform, &shape);

    assert_eq!(
        codes(&report),
        Vec::<String>::new(),
        "EUR/eu and USD/eu are two markets and may differ"
    );
}

/// **D-132's carve-out.** A live `existing_grandfathered` generation carrying the
/// other basis does not make the market mixed.
///
/// Without it, one cutover freezes a market's display basis **permanently**: the
/// generation is immutable, MUST NOT be superseded and never leaves `published`,
/// so every later publish would fail on a divergent row nobody can fix. Its
/// subscribers read the basis from their own frozen snapshot, so invoice
/// coherence is unaffected.
#[test]
fn a_grandfathered_generation_is_not_in_the_uniformity_row_set() {
    let shape = shape_of(vec![
        row(
            0xb1,
            "EUR",
            "eu",
            true,
            Some("s"),
            PriceEligibility::AllSubscriptions,
        ),
        row(
            0xb2,
            "EUR",
            "eu",
            false,
            Some("s"),
            PriceEligibility::ExistingGrandfathered,
        ),
    ]);

    let report = run(&MarketBasisUniform, &shape);

    assert_eq!(
        codes(&report),
        Vec::<String>::new(),
        "the immutable generation must not freeze the market's basis (D-132)"
    );
}

/// The carve-out does not extend to `new_subscriptions_only`, which D-132 does
/// not name and which is neither immutable nor unsupersedable.
#[test]
fn a_new_subscriptions_only_row_is_still_in_the_uniformity_row_set() {
    let shape = shape_of(vec![
        row(
            0xb1,
            "EUR",
            "eu",
            true,
            Some("s"),
            PriceEligibility::AllSubscriptions,
        ),
        row(
            0xb2,
            "EUR",
            "eu",
            false,
            Some("s"),
            PriceEligibility::NewSubscriptionsOnly,
        ),
    ]);

    let report = run(&MarketBasisUniform, &shape);

    assert_eq!(codes(&report), [TAX_BASIS_MIXED_MARKET]);
}

/// The rule names the instruction it implements.
#[test]
fn the_rules_name_the_instructions_they_implement() {
    assert_eq!(MarketBasisUniform.name(), "inst-td-basis-uniform");
    assert_eq!(
        policy(TaxDisplayPolicy::FailClosed, RegionTaxReadiness::default()).name(),
        "inst-td-policy"
    );
}
