//! Tests for the billing-cycle shape rules.
//!
//! Every rule here has arms, and an arm is what a test covers: a rule whose
//! test only walks the happy path proves that the rule compiles. So each
//! boundary is pinned on both sides (366 days passes and 367 fails, 24 months
//! passes and 25 fails), each missing part of a hybrid is named separately, and
//! D-84 is exercised through the two shapes that hid the defect — a hybrid
//! whose usage reaches one of two sold markets, and a usage-only plan pricing
//! two meters where only one of them reaches both.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{
    AvailableFromNotBackdated, BaseMarketCompleteness, CustomIntervalBounds, CycleDeclared,
    HybridCompleteness, PurchaseQtyRange, SetupRowShape, UsageMarketCompleteness,
};
use crate::domain::concurrency::RowVersion;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount, RateMinor};
use crate::domain::plan_rules::{
    AVAILABLE_FROM_IN_PAST, BASE_MARKET_INCOMPLETE, CYCLE_METADATA_MISSING, HYBRID_INCOMPLETE,
    INVALID_CUSTOM_INTERVAL, PURCHASE_QTY_RANGE_INVALID, SETUP_ROW_INVALID,
    USAGE_MARKET_INCOMPLETE,
};
use crate::domain::plan_shape::{
    BillingCycle, CustomIntervalUnit, Frequency, PhaseGraph, PhaseKind, PlanPhase, PlanShape,
    PublishedBaseline,
};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{
    ModelKind, PriceRow, TierAggregationWindow, TierBand, TierQualificationWindow,
};
use crate::domain::rules::model_kind::{KindChargeKindMatrix, KindRequiredFields};
use crate::domain::rules::{AMOUNT_PLACEMENT_INVALID, MODEL_KIND_CHARGEKIND_MISMATCH};
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::validation::{ValidationReport, ValidationRule};
use time::Duration;
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The two configured caps, transcribed from `LimitsConfig::default` and PRD
/// §14 rather than read from it: a test that took the caps from the same value
/// the rule takes them from could not notice either one moving.
const MAX_DAYS: u32 = 366;
/// The months cap; see [`MAX_DAYS`].
const MAX_MONTHS: u32 = 24;

/// The plan's terminal phase — the one usage rows are phase-invariant on.
const TERMINAL: u128 = 0x7e_11;
/// A trial phase, the home of every phase-scoped override in these tests.
const TRIAL: u128 = 0x77_1a;

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x91a4))
}

fn phase_id(seed: u128) -> PhaseId {
    PhaseId::new(Uuid::from_u128(seed))
}

fn now() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, 12, 0, 0)
}

fn minor(units: i64) -> MinorAmount {
    MinorAmount::new(units).expect("test amount is non-negative")
}

/// A band rate, stated in whole minor units so these cases read as they
/// always did (D-311). The stored scale is 10^-9 of one.
fn rate(minor_units: i64) -> RateMinor {
    RateMinor::from_minor_units(minor_units).expect("test rate is non-negative")
}

fn scope_key(charge_kind: ChargeKind, code: &str, market: &str, on_phase: PhaseId) -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new(code).expect("test currency is three letters"),
        Region::new(market).expect("test region is non-blank"),
        on_phase,
        PriceEligibility::AllSubscriptions,
        charge_kind,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
}

fn record(
    seed: u128,
    charge_kind: ChargeKind,
    code: &str,
    market: &str,
    on_phase: PhaseId,
) -> PriceRecord {
    PriceRecord {
        price_id: Uuid::from_u128(seed),
        scope_key: scope_key(charge_kind, code, market, on_phase),
        row: PriceRow::new(charge_kind, Some(ModelKind::Flat)),
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

/// A recurring base row in one market.
fn recurring(seed: u128, code: &str, market: &str) -> PriceRecord {
    let mut base = record(
        seed,
        ChargeKind::Recurring,
        code,
        market,
        phase_id(TERMINAL),
    );
    base.row.amount_minor = Some(minor(2_500));
    base
}

/// A usage row pricing one `(meter, dimensionKey)` line in one market.
///
/// **`per_unit` and therefore `unit_rate`.** The kind is not a free choice here:
/// `flat` on a usage key is `MODEL_KIND_CHARGEKIND_MISMATCH`, so a usage row is
/// `per_unit` and prices from the rate column (D-311). A fixture keeping this
/// money in `amount_minor` is refused twice by `inst-mk-required` — a forbidden
/// column carrying money and the priced column absent — which would make every
/// D-84 conclusion below a conclusion about a row no publish can carry.
/// [`the_usage_fixtures_are_rows_a_publish_could_carry`] holds that.
fn usage(seed: u128, code: &str, market: &str, meter: &str, on_phase: PhaseId) -> PriceRecord {
    let mut metered = record(seed, ChargeKind::Usage, code, market, on_phase);
    metered.row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::PerUnit));
    metered.row.meter = Some(meter.to_owned());
    metered.row.unit_rate = Some(rate(3));
    metered
}

/// The same line at an explicit zero — how a market where usage is genuinely
/// free is authored (Slice 3 Q5), and the only thing that satisfies D-84 there.
///
/// The zero sits in the column the kind prices from, [`usage`]'s reason: a
/// `per_unit` line that is free is a rate of zero, and a zero in `amount_minor`
/// would be an unpublishable row claiming to be the authored `$0` D-84 asks for.
fn free_usage(seed: u128, code: &str, market: &str, meter: &str) -> PriceRecord {
    let mut metered = usage(seed, code, market, meter, phase_id(TERMINAL));
    metered.row.unit_rate = Some(rate(0));
    metered
}

/// A setup row, shaped legally.
fn setup(seed: u128) -> PriceRecord {
    let mut fee = record(
        seed,
        ChargeKind::OneTimeSetup,
        "usd",
        "US",
        phase_id(TERMINAL),
    );
    fee.row.amount_minor = Some(minor(9_900));
    fee
}

/// A plan whose only phase is the terminal one — the non-phased shape
/// `inst-ph-default` auto-creates.
fn terminal_graph() -> PhaseGraph {
    PhaseGraph::new(vec![PlanPhase {
        phase_id: phase_id(TERMINAL),
        kind: PhaseKind::Evergreen,
        display_name: None,
        ordinal: 1,
        converts_to_phase_id: None,
        phase_duration_days: None,
        display_trial_days: None,
    }])
}

/// A trial converting into the terminal phase.
fn trial_then_terminal_graph() -> PhaseGraph {
    PhaseGraph::new(vec![
        PlanPhase {
            phase_id: phase_id(TRIAL),
            kind: PhaseKind::Trial,
            display_name: None,
            ordinal: 0,
            converts_to_phase_id: Some(phase_id(TERMINAL)),
            phase_duration_days: Some(14),
            display_trial_days: Some(14),
        },
        PlanPhase {
            phase_id: phase_id(TERMINAL),
            kind: PhaseKind::Evergreen,
            display_name: None,
            ordinal: 1,
            converts_to_phase_id: None,
            phase_duration_days: None,
            display_trial_days: None,
        },
    ])
}

fn shape(cycle: BillingCycle) -> PlanShape {
    let mut subject = PlanShape::new(plan(), 3, now());
    subject.billing_cycle = Some(cycle);
    subject.phases = terminal_graph();
    subject
}

fn findings(rule: &impl ValidationRule<PlanShape>, subject: &PlanShape) -> ValidationReport {
    let mut report = ValidationReport::default();
    rule.evaluate(subject, &mut report);
    report
}

/// [`findings`] one subject down, for the row rules this file's fixtures have to
/// survive without being what this file is about.
fn row_findings(rule: &impl ValidationRule<PriceRow>, row: &PriceRow) -> ValidationReport {
    let mut report = ValidationReport::default();
    rule.evaluate(row, &mut report);
    report
}

fn codes(report: &ValidationReport) -> Vec<&str> {
    report
        .violations
        .iter()
        .map(|violation| violation.code.as_str())
        .collect()
}

fn details(report: &ValidationReport) -> String {
    report
        .violations
        .iter()
        .map(|violation| format!("{}|{}", violation.subject, violation.detail))
        .collect::<Vec<_>>()
        .join("\n")
}

fn custom(n: u32, unit: CustomIntervalUnit) -> PlanShape {
    let mut subject = shape(BillingCycle::Recurring);
    subject.frequency = Some(Frequency::CustomEveryN { n, unit });
    subject
}

fn bounds() -> CustomIntervalBounds {
    CustomIntervalBounds::new(MAX_DAYS, MAX_MONTHS)
}

// ---------------------------------------------------------------------------
// inst-cs-customfreq
// ---------------------------------------------------------------------------

#[test]
fn a_non_positive_custom_interval_fails() {
    let report = findings(&bounds(), &custom(0, CustomIntervalUnit::Days));

    assert_eq!(codes(&report), vec![INVALID_CUSTOM_INTERVAL]);
    assert!(
        details(&report).contains("days(0)"),
        "the report names n and its unit"
    );
}

#[test]
fn the_days_cap_is_inclusive_and_an_over_cap_interval_is_rejected_not_clamped() {
    // 366 is the ratified cap, so it publishes; 367 fails and the plan keeps
    // the interval its author wrote. A clamp would publish a billing period
    // nobody chose and record nothing about the substitution.
    let at_cap = findings(&bounds(), &custom(MAX_DAYS, CustomIntervalUnit::Days));
    assert!(at_cap.is_publishable());

    let over_cap = findings(&bounds(), &custom(MAX_DAYS + 1, CustomIntervalUnit::Days));
    assert_eq!(codes(&over_cap), vec![INVALID_CUSTOM_INTERVAL]);
    let detail = details(&over_cap);
    assert!(
        detail.contains("days(367)"),
        "the report names n and the unit"
    );
    assert!(
        detail.contains("366"),
        "the report names the cap it applied"
    );
    assert!(
        detail.contains("never clamped"),
        "the refusal states that the value was not substituted"
    );
}

#[test]
fn the_months_cap_is_its_own_bound() {
    // The two units do not share a cap: 25 days is unremarkable and 25 months
    // is over the ratified bound, so the rule reads the unit before the bound.
    let at_cap = findings(&bounds(), &custom(MAX_MONTHS, CustomIntervalUnit::Months));
    assert!(at_cap.is_publishable());

    let over_cap = findings(
        &bounds(),
        &custom(MAX_MONTHS + 1, CustomIntervalUnit::Months),
    );
    assert_eq!(codes(&over_cap), vec![INVALID_CUSTOM_INTERVAL]);
    assert!(details(&over_cap).contains("months(25)"));

    let same_n_in_days = findings(&bounds(), &custom(MAX_MONTHS + 1, CustomIntervalUnit::Days));
    assert!(
        same_n_in_days.is_publishable(),
        "25 is over the months cap and well inside the days cap"
    );
}

#[test]
fn a_fixed_frequency_and_an_unauthored_one_are_not_this_rules_business() {
    let mut fixed = shape(BillingCycle::Recurring);
    fixed.frequency = Some(Frequency::Quarterly);
    assert!(findings(&bounds(), &fixed).is_publishable());

    let unauthored = shape(BillingCycle::Recurring);
    assert!(findings(&bounds(), &unauthored).is_publishable());
}

// ---------------------------------------------------------------------------
// inst-cs-hybrid
// ---------------------------------------------------------------------------

#[test]
fn a_hybrid_without_a_usage_row_names_the_missing_usage_part() {
    let mut subject = shape(BillingCycle::Hybrid);
    subject.rows = vec![recurring(0xb1, "usd", "US")];

    let report = findings(&HybridCompleteness, &subject);

    assert_eq!(codes(&report), vec![HYBRID_INCOMPLETE]);
    assert!(details(&report).contains("the usage part is missing"));
}

#[test]
fn a_hybrid_without_a_recurring_row_names_the_missing_recurring_part() {
    let mut subject = shape(BillingCycle::Hybrid);
    subject.rows = vec![usage(0xb2, "usd", "US", "api_calls", phase_id(TERMINAL))];

    let report = findings(&HybridCompleteness, &subject);

    assert_eq!(codes(&report), vec![HYBRID_INCOMPLETE]);
    assert!(details(&report).contains("the recurring part is missing"));
}

#[test]
fn a_hybrid_missing_both_parts_reports_both() {
    // The author remediates a plan in one pass, so a report that named only
    // the first missing part would send them round the pipeline twice.
    let subject = shape(BillingCycle::Hybrid);

    let report = findings(&HybridCompleteness, &subject);

    assert_eq!(codes(&report), vec![HYBRID_INCOMPLETE, HYBRID_INCOMPLETE]);
    let detail = details(&report);
    assert!(detail.contains("the recurring part is missing"));
    assert!(detail.contains("the usage part is missing"));
}

#[test]
fn a_complete_hybrid_passes_and_the_other_cycles_are_out_of_scope() {
    let mut complete = shape(BillingCycle::Hybrid);
    complete.rows = vec![
        recurring(0xb3, "usd", "US"),
        usage(0xb4, "usd", "US", "api_calls", phase_id(TERMINAL)),
    ];
    assert!(findings(&HybridCompleteness, &complete).is_publishable());

    // A recurring plan with no usage row is not an incomplete hybrid, and a
    // usage-only plan with no recurring row is not one either.
    let mut recurring_only = shape(BillingCycle::Recurring);
    recurring_only.rows = vec![recurring(0xb5, "usd", "US")];
    assert!(findings(&HybridCompleteness, &recurring_only).is_publishable());

    let mut usage_only = shape(BillingCycle::Usage);
    usage_only.rows = vec![usage(0xb6, "usd", "US", "api_calls", phase_id(TERMINAL))];
    assert!(findings(&HybridCompleteness, &usage_only).is_publishable());
}

// ---------------------------------------------------------------------------
// inst-cs-usage / inst-cs-hybrid (D-84)
// ---------------------------------------------------------------------------

#[test]
fn a_hybrid_selling_two_markets_with_usage_in_one_fails_and_a_zero_row_fixes_it() {
    // The defect verbatim: recurring in EUR and USD, usage only in EUR. The
    // plan is sellable in USD and the USD subscriber's usage events fail
    // closed — "sold but unrateable".
    let mut subject = shape(BillingCycle::Hybrid);
    subject.rows = vec![
        recurring(0xc1, "eur", "EU"),
        recurring(0xc2, "usd", "US"),
        usage(0xc3, "eur", "EU", "api_calls", phase_id(TERMINAL)),
    ];

    let report = findings(&UsageMarketCompleteness, &subject);

    assert_eq!(codes(&report), vec![USAGE_MARKET_INCOMPLETE]);
    let detail = details(&report);
    assert!(detail.contains("api_calls"), "the report names the meter");
    assert!(detail.contains("USD/US"), "the report names the market");

    // A market where usage is genuinely free is an explicit 0 row, never an
    // absence. That is the sentence that tells the author how to satisfy it.
    subject
        .rows
        .push(free_usage(0xc4, "usd", "US", "api_calls"));
    assert!(findings(&UsageMarketCompleteness, &subject).is_publishable());
}

/// Every fixture this file draws a plan-level conclusion from is a row a publish
/// could actually carry.
///
/// [`UsageMarketCompleteness`] reads the meter, the dimension key and the scope
/// key and touches no money column at all, so a fixture keeping its money in the
/// wrong one satisfies every case here while `inst-mk-required` — a rule this file
/// never runs — refuses it. The D-84 cases would then be conclusions about rows no
/// author can publish.
///
/// **Moving the kind is not the way out, and that is why the money moved.** An
/// authored `$0` expressed as `flat` + `amount_minor` puts the money where a
/// `flat` row keeps it and is still unpublishable one rule over: `flat` on a usage
/// key is `MODEL_KIND_CHARGEKIND_MISMATCH`, because a usage row takes its quantity
/// from the meter and a flat sum multiplies nothing. Both refusals are asserted,
/// so neither half of that pair can quietly stop holding.
#[test]
fn the_usage_fixtures_are_rows_a_publish_could_carry() {
    for record in [
        usage(0x9001, "usd", "US", "api_calls", phase_id(TERMINAL)),
        free_usage(0x9002, "usd", "US", "api_calls"),
        // The other two builders, on the same claim: a `flat` row's money
        // belongs in `amount_minor`, and a rule that had stopped judging
        // anything would satisfy the usage half above on its own.
        recurring(0x9003, "usd", "US"),
        setup(0x9004),
    ] {
        assert_eq!(
            codes(&row_findings(&KindRequiredFields, &record.row)),
            Vec::<&str>::new(),
            "fixture {} prices from a column its kind does not price from",
            record.price_id
        );
    }

    let mut misplaced = usage(0x9005, "usd", "US", "api_calls", phase_id(TERMINAL)).row;
    misplaced.unit_rate = None;
    misplaced.amount_minor = Some(minor(3));
    assert_eq!(
        codes(&row_findings(&KindRequiredFields, &misplaced)),
        vec![AMOUNT_PLACEMENT_INVALID, AMOUNT_PLACEMENT_INVALID],
        "control: money in a forbidden column, and the column the kind prices from absent"
    );

    misplaced.model_kind = Some(ModelKind::Flat);
    assert_eq!(
        codes(&row_findings(&KindChargeKindMatrix, &misplaced)),
        vec![MODEL_KIND_CHARGEKIND_MISMATCH],
        "control: and calling the row `flat` to legalise the column refuses it on the key"
    );
}

#[test]
fn the_hybrid_market_set_is_the_recurring_rows_markets() {
    // Both spellings are normative and they differ. On a hybrid the set is
    // every market where a recurring row exists, so a usage row in a market the
    // plan does not sell recurring in does not enlarge the obligation.
    let mut subject = shape(BillingCycle::Hybrid);
    subject.rows = vec![
        recurring(0xc5, "eur", "EU"),
        usage(0xc6, "eur", "EU", "api_calls", phase_id(TERMINAL)),
        usage(0xc7, "gbp", "UK", "api_calls", phase_id(TERMINAL)),
    ];

    assert!(findings(&UsageMarketCompleteness, &subject).is_publishable());
}

#[test]
fn a_usage_only_plan_pricing_two_meters_owes_every_market_for_each_of_them() {
    // A plan MAY price several meteringUnits (D-103), so M2 is legal — and
    // being legal is exactly why its coverage has to be checked. The market set
    // here is the union of the usage rows' markets.
    //
    // **This case was a rule about a subject the store could not hold, and since
    // D-196 it is not** (clause 4, 2026-08-06). The canonical scope key carried
    // no `meter`, so the two `eur/EU` rows below rendered **one** key and the
    // second was refused `DUPLICATE_SCOPE_KEY` at authoring — this test asserted
    // publishability for a shape no plan could reach, and D-196's entry recorded
    // it as such while the fork was open. The key now carries the line, and
    // `sqlite_price_repo::two_usage_lines_of_one_market_both_author` is the same
    // pair going through the door it used to bounce off.
    let mut subject = shape(BillingCycle::Usage);
    subject.rows = vec![
        usage(0xd1, "eur", "EU", "cloudlets", phase_id(TERMINAL)),
        usage(0xd2, "usd", "US", "cloudlets", phase_id(TERMINAL)),
        usage(0xd3, "eur", "EU", "egress_gb", phase_id(TERMINAL)),
    ];

    let report = findings(&UsageMarketCompleteness, &subject);

    assert_eq!(codes(&report), vec![USAGE_MARKET_INCOMPLETE]);
    let detail = details(&report);
    assert!(detail.contains("egress_gb"), "the report names the meter");
    assert!(detail.contains("USD/US"), "the report names the market");
    assert!(
        !detail.contains("cloudlets"),
        "the complete line is not reported"
    );

    subject
        .rows
        .push(usage(0xd4, "usd", "US", "egress_gb", phase_id(TERMINAL)));
    assert!(
        findings(&UsageMarketCompleteness, &subject).is_publishable(),
        "a plan pricing several meters publishes once every line reaches every market"
    );
}

#[test]
fn a_dimension_key_splits_a_meter_into_lines_of_its_own() {
    // The line is `(meter, dimensionKey)`, not the meter: a plan pricing
    // storage by class owes every market for every class it prices.
    let mut subject = shape(BillingCycle::Usage);
    let mut hot = usage(0xd5, "eur", "EU", "storage_gb", phase_id(TERMINAL));
    hot.row.dimension_key = "hot".to_owned();
    let mut hot_us = usage(0xd6, "usd", "US", "storage_gb", phase_id(TERMINAL));
    hot_us.row.dimension_key = "hot".to_owned();
    let mut cold = usage(0xd7, "eur", "EU", "storage_gb", phase_id(TERMINAL));
    cold.row.dimension_key = "cold".to_owned();
    subject.rows = vec![hot, hot_us, cold];

    let report = findings(&UsageMarketCompleteness, &subject);

    assert_eq!(codes(&report), vec![USAGE_MARKET_INCOMPLETE]);
    let detail = details(&report);
    assert!(
        detail.contains("cold"),
        "the report names the dimension key"
    );
    assert!(detail.contains("USD/US"));
}

#[test]
fn a_phase_scoped_override_neither_adds_a_line_nor_discharges_a_market() {
    // Phase-scoped overrides are additive and exempt (D-15/D-19). The trial
    // row prices a second meter in one market only, and it does not become a
    // market obligation — otherwise every free-trial row would drag a plan into
    // a completeness failure it has no way to satisfy.
    let mut subject = shape(BillingCycle::Usage);
    subject.phases = trial_then_terminal_graph();
    subject.rows = vec![
        usage(0xe1, "eur", "EU", "cloudlets", phase_id(TERMINAL)),
        usage(0xe2, "usd", "US", "cloudlets", phase_id(TERMINAL)),
        usage(0xe3, "eur", "EU", "trial_probe", phase_id(TRIAL)),
    ];

    assert!(findings(&UsageMarketCompleteness, &subject).is_publishable());

    // The other half of "exempt": an override does not count as coverage
    // either. After the phase converts it resolves to nothing, so a line whose
    // only row in a market is phase-scoped is still incomplete there.
    let mut only_scoped = shape(BillingCycle::Usage);
    only_scoped.phases = trial_then_terminal_graph();
    only_scoped.rows = vec![
        usage(0xe4, "eur", "EU", "cloudlets", phase_id(TERMINAL)),
        usage(0xe5, "usd", "US", "cloudlets", phase_id(TRIAL)),
    ];

    let report = findings(&UsageMarketCompleteness, &only_scoped);
    assert_eq!(codes(&report), vec![USAGE_MARKET_INCOMPLETE]);
    assert!(details(&report).contains("USD/US"));
}

#[test]
fn the_rule_defers_to_the_phase_graph_when_no_single_terminal_phase_exists() {
    // "The terminal-phase rows" has no referent on a graph with zero or two
    // terminals, and `PHASE_GRAPH_INVALID` has already recorded that fault.
    // The publish stays blocked by that violation, so nothing is let through.
    let mut subject = shape(BillingCycle::Usage);
    subject.phases = PhaseGraph::default();
    subject.rows = vec![
        usage(0xe6, "eur", "EU", "cloudlets", phase_id(TERMINAL)),
        usage(0xe7, "usd", "US", "egress_gb", phase_id(TERMINAL)),
    ];

    assert!(findings(&UsageMarketCompleteness, &subject).is_publishable());
}

#[test]
fn one_time_and_recurring_plans_owe_no_metered_part() {
    let mut one_time = shape(BillingCycle::OneTime);
    one_time.rows = vec![record(
        0xe8,
        ChargeKind::OneTime,
        "usd",
        "US",
        phase_id(TERMINAL),
    )];
    assert!(findings(&UsageMarketCompleteness, &one_time).is_publishable());

    let mut recurring_only = shape(BillingCycle::Recurring);
    recurring_only.rows = vec![recurring(0xe9, "usd", "US"), recurring(0xea, "eur", "EU")];
    assert!(findings(&UsageMarketCompleteness, &recurring_only).is_publishable());
}

// ---------------------------------------------------------------------------
// inst-cs-setup
// ---------------------------------------------------------------------------

#[test]
fn a_setup_row_on_a_one_time_plan_fails() {
    let mut subject = shape(BillingCycle::OneTime);
    subject.rows = vec![setup(0xf1)];

    let report = findings(&SetupRowShape, &subject);

    assert_eq!(codes(&report), vec![SETUP_ROW_INVALID]);
    let detail = details(&report);
    assert!(
        detail.contains(&Uuid::from_u128(0xf1).to_string()),
        "the report names the offending row"
    );
    assert!(
        detail.contains("cycle is one_time"),
        "the report names the cycle that refused it"
    );
}

#[test]
fn a_setup_row_on_a_usage_only_plan_fails() {
    let mut subject = shape(BillingCycle::Usage);
    subject.rows = vec![setup(0xf2)];

    assert_eq!(
        codes(&findings(&SetupRowShape, &subject)),
        vec![SETUP_ROW_INVALID]
    );
}

#[test]
fn a_setup_row_carrying_billing_timing_fails() {
    // Slice 6 owns "required on a recurring row"; this owns "forbidden on a
    // setup row". A setup fee has no billing period to be in advance of.
    let mut subject = shape(BillingCycle::Recurring);
    let mut fee = setup(0xf3);
    fee.billing_timing = Some("advance".to_owned());
    subject.rows = vec![fee];

    let report = findings(&SetupRowShape, &subject);

    assert_eq!(codes(&report), vec![SETUP_ROW_INVALID]);
    assert!(details(&report).contains("billingTiming"));
}

#[test]
fn a_setup_row_carrying_tier_machinery_names_every_field_it_carries() {
    let mut subject = shape(BillingCycle::Hybrid);
    let mut fee = setup(0xf4);
    fee.row.bands = vec![TierBand::open(0, rate(100))];
    fee.row.tier_aggregation_window = Some(TierAggregationWindow::CalendarMonth);
    fee.row.tier_qualification_window = Some(TierQualificationWindow::Current);
    subject.rows = vec![fee];

    let report = findings(&SetupRowShape, &subject);

    assert_eq!(codes(&report), vec![SETUP_ROW_INVALID]);
    let detail = details(&report);
    assert!(detail.contains("tier bands"));
    assert!(detail.contains("tierAggregationWindow"));
    assert!(detail.contains("tierQualificationWindow"));
}

#[test]
fn each_forbidden_field_fails_on_its_own() {
    // One arm per field, because a rule that only ever sees all four at once
    // would pass with three of the four checks deleted.
    let bands = {
        let mut fee = setup(0xf5);
        fee.row.bands = vec![TierBand::open(0, rate(100))];
        fee
    };
    let aggregation = {
        let mut fee = setup(0xf6);
        fee.row.tier_aggregation_window = Some(TierAggregationWindow::InvoicePeriod);
        fee
    };
    let qualification = {
        let mut fee = setup(0xf7);
        fee.row.tier_qualification_window = Some(TierQualificationWindow::TrailingPeriod);
        fee
    };

    for (fee, expected) in [
        (bands, "tier bands"),
        (aggregation, "tierAggregationWindow"),
        (qualification, "tierQualificationWindow"),
    ] {
        let mut subject = shape(BillingCycle::Recurring);
        subject.rows = vec![fee];

        let report = findings(&SetupRowShape, &subject);

        assert_eq!(codes(&report), vec![SETUP_ROW_INVALID], "{expected}");
        assert!(details(&report).contains(expected));
    }
}

#[test]
fn a_clean_setup_row_on_a_hybrid_plan_passes() {
    let mut subject = shape(BillingCycle::Hybrid);
    subject.rows = vec![
        recurring(0xf8, "usd", "US"),
        usage(0xf9, "usd", "US", "api_calls", phase_id(TERMINAL)),
        setup(0xfa),
    ];

    assert!(findings(&SetupRowShape, &subject).is_publishable());
}

#[test]
fn a_draft_with_no_authored_cycle_is_not_judged_on_where_its_setup_row_lives() {
    // A plan is authored incrementally while it is draft; rejecting the row
    // for a cycle nobody chose would report a fault the author did not make.
    // The field half still binds, because it needs no cycle to be true.
    let mut subject = PlanShape::new(plan(), 3, now());
    subject.phases = terminal_graph();
    subject.rows = vec![setup(0xfb)];
    assert!(findings(&SetupRowShape, &subject).is_publishable());

    let mut fee = setup(0xfc);
    fee.billing_timing = Some("arrears".to_owned());
    subject.rows = vec![fee];
    assert_eq!(
        codes(&findings(&SetupRowShape, &subject)),
        vec![SETUP_ROW_INVALID]
    );
}

// ---------------------------------------------------------------------------
// inst-cs-onetime
// ---------------------------------------------------------------------------

#[test]
fn an_inverted_purchase_quantity_window_fails() {
    let mut subject = shape(BillingCycle::OneTime);
    subject.purchase_min_qty = Some(10);
    subject.purchase_max_qty = Some(2);

    let report = findings(&PurchaseQtyRange, &subject);

    assert_eq!(codes(&report), vec![PURCHASE_QTY_RANGE_INVALID]);
    let detail = details(&report);
    assert!(detail.contains("purchase_min_qty 10"));
    assert!(detail.contains("purchase_max_qty 2"));
}

#[test]
fn an_equal_a_wider_and_a_half_authored_window_all_pass() {
    let mut equal = shape(BillingCycle::OneTime);
    equal.purchase_min_qty = Some(5);
    equal.purchase_max_qty = Some(5);
    assert!(findings(&PurchaseQtyRange, &equal).is_publishable());

    let mut wider = shape(BillingCycle::OneTime);
    wider.purchase_min_qty = Some(1);
    wider.purchase_max_qty = Some(100);
    assert!(findings(&PurchaseQtyRange, &wider).is_publishable());

    // Both bounds are optional, and one bound alone bounds nothing to compare.
    let mut floor_only = shape(BillingCycle::OneTime);
    floor_only.purchase_min_qty = Some(99);
    assert!(findings(&PurchaseQtyRange, &floor_only).is_publishable());

    let mut ceiling_only = shape(BillingCycle::OneTime);
    ceiling_only.purchase_max_qty = Some(1);
    assert!(findings(&PurchaseQtyRange, &ceiling_only).is_publishable());
}

// ---------------------------------------------------------------------------
// inst-cs-availability
// ---------------------------------------------------------------------------

#[test]
fn a_newly_set_past_available_from_fails_on_every_cycle() {
    // Cycle-independent since the 2026-07-28 fix: leaving the rule in the
    // one-time step left three of the four cycles unguarded.
    let backdated = now() - Duration::days(30);

    for cycle in BillingCycle::ALL {
        let mut subject = shape(*cycle);
        subject.available_from = Some(backdated);

        let report = findings(&AvailableFromNotBackdated, &subject);

        assert_eq!(codes(&report), vec![AVAILABLE_FROM_IN_PAST], "{cycle}");
    }
}

#[test]
fn a_re_published_unchanged_available_from_that_has_since_passed_passes() {
    // The qualifier is the whole rule: without it, a descriptor fix or a new
    // market on a once-future-dated plan is blocked until the operator erases
    // a date that was correct when it was authored.
    let authored = now() - Duration::days(30);
    let mut subject = shape(BillingCycle::Recurring);
    subject.available_from = Some(authored);
    subject.baseline = Some(PublishedBaseline {
        terminal_phase_id: phase_id(TERMINAL),
        phase_ids_in_use: [phase_id(TERMINAL)].into(),
        available_from: Some(authored),
        available_to: None,
    });

    assert!(findings(&AvailableFromNotBackdated, &subject).is_publishable());
}

#[test]
fn a_changed_past_available_from_still_fails_against_a_baseline() {
    // Unchanged is what is forgiven, not "the plan has published before".
    let published = now() - Duration::days(30);
    let mut subject = shape(BillingCycle::Recurring);
    subject.available_from = Some(now() - Duration::days(60));
    subject.baseline = Some(PublishedBaseline {
        terminal_phase_id: phase_id(TERMINAL),
        phase_ids_in_use: [phase_id(TERMINAL)].into(),
        available_from: Some(published),
        available_to: None,
    });

    let report = findings(&AvailableFromNotBackdated, &subject);

    assert_eq!(codes(&report), vec![AVAILABLE_FROM_IN_PAST]);
}

#[test]
fn a_first_publish_setting_a_past_date_fails_and_a_future_date_passes() {
    let mut backdated = shape(BillingCycle::Recurring);
    backdated.available_from = Some(now() - Duration::seconds(1));
    assert!(
        backdated.baseline.is_none(),
        "a first publish has no baseline"
    );
    assert_eq!(
        codes(&findings(&AvailableFromNotBackdated, &backdated)),
        vec![AVAILABLE_FROM_IN_PAST]
    );

    let mut dated_forward = shape(BillingCycle::Recurring);
    dated_forward.available_from = Some(now() + Duration::days(7));
    assert!(findings(&AvailableFromNotBackdated, &dated_forward).is_publishable());

    // The evaluation instant itself is not the past.
    let mut at_the_instant = shape(BillingCycle::Recurring);
    at_the_instant.available_from = Some(now());
    assert!(findings(&AvailableFromNotBackdated, &at_the_instant).is_publishable());

    // No authored date is not a backdated one.
    let unauthored = shape(BillingCycle::Recurring);
    assert!(findings(&AvailableFromNotBackdated, &unauthored).is_publishable());
}

#[test]
fn a_baseline_that_carried_no_date_makes_a_past_one_newly_set() {
    let mut subject = shape(BillingCycle::Recurring);
    subject.available_from = Some(now() - Duration::days(1));
    subject.baseline = Some(PublishedBaseline {
        terminal_phase_id: phase_id(TERMINAL),
        phase_ids_in_use: [phase_id(TERMINAL)].into(),
        available_from: None,
        available_to: None,
    });

    assert_eq!(
        codes(&findings(&AvailableFromNotBackdated, &subject)),
        vec![AVAILABLE_FROM_IN_PAST]
    );
}

// ---------------------------------------------------------------------------
// inst-cs-declared (D-149 clause 2)
// ---------------------------------------------------------------------------

/// A plan with a clean shape and **no cycle at all** — the state that used to
/// reach publish unjudged.
fn cycleless() -> PlanShape {
    let mut subject = PlanShape::new(plan(), 3, now());
    subject.phases = terminal_graph();
    subject.rows = vec![recurring(0xb001, "usd", "US")];
    subject
}

#[test]
fn a_plan_with_no_cycle_is_refused_by_name_rather_than_passing_vacuously() {
    // The point of the rule. Delete `CycleDeclared` from `plan_shape_rules` and
    // this fails - and so does
    // `the_whole_cycle_shape_step_passes_a_cycleless_plan_without_this_rule`
    // below, which is the same fact stated over the step rather than the rule.
    let report = findings(&CycleDeclared, &cycleless());

    assert_eq!(codes(&report), vec![CYCLE_METADATA_MISSING]);
    assert!(
        details(&report).contains("billingCycle"),
        "the report names the field: {}",
        details(&report)
    );
}

#[test]
fn the_whole_cycle_shape_step_passes_a_cycleless_plan_without_this_rule() {
    // The arithmetic that made the gap: five rules, each correctly declining to
    // judge a plan whose cycle it cannot read, adding up to no judgement. Every
    // other rule of the step is run here against the same cycleless plan and
    // every one of them says nothing.
    let subject = cycleless();

    for report in [
        findings(&bounds(), &subject),
        findings(&HybridCompleteness, &subject),
        findings(&BaseMarketCompleteness, &subject),
        findings(&UsageMarketCompleteness, &subject),
        findings(&SetupRowShape, &subject),
        findings(&PurchaseQtyRange, &subject),
        findings(&AvailableFromNotBackdated, &subject),
    ] {
        assert!(
            report.violations.is_empty(),
            "each rule of the step correctly declines: {:?}",
            codes(&report)
        );
    }
}

#[test]
fn a_recurring_plan_with_no_frequency_is_refused_naming_that_field_instead() {
    // One code, two fields (D-146's line): the operator's next action is the
    // same - author the field the plan is missing - and the report says which.
    let mut subject = shape(BillingCycle::Recurring);
    subject.rows = vec![recurring(0xb001, "usd", "US")];

    let report = findings(&CycleDeclared, &subject);

    assert_eq!(codes(&report), vec![CYCLE_METADATA_MISSING]);
    assert!(
        details(&report).contains("frequency"),
        "{}",
        details(&report)
    );
}

#[test]
fn a_cycle_that_does_not_recur_owes_no_frequency() {
    // Scoped by `has_recurring_part`, never by a cycle list re-spelled in the
    // rule: a one-time or usage-only plan has no recurrence to describe, and a
    // rule demanding one would be a fault the author cannot fix.
    for cycle in [BillingCycle::OneTime, BillingCycle::Usage] {
        let report = findings(&CycleDeclared, &shape(cycle));
        assert!(
            report.violations.is_empty(),
            "{cycle}: {:?}",
            codes(&report)
        );
    }
}

#[test]
fn a_first_publish_with_no_baseline_is_not_a_fault_this_rule_reports() {
    // The `inst-cs-declared` posture, and the case the G5 plan flagged: the rule
    // asks what the plan says about itself, not what it used to say. A plan with
    // a declared cycle and frequency and NO published baseline is a clean first
    // publish.
    let mut subject = shape(BillingCycle::Recurring);
    subject.frequency = Some(Frequency::Monthly);
    subject.rows = vec![recurring(0xb001, "usd", "US")];
    assert!(subject.baseline.is_none(), "this is a first publish");

    assert!(findings(&CycleDeclared, &subject).violations.is_empty());
}

// ---------------------------------------------------------------------------
// inst-cs-onetime / inst-cs-recurring (D-149 clause 1)
// ---------------------------------------------------------------------------

/// A recurring plan selling in two markets with a base row in only one of them.
fn recurring_missing_a_market() -> PlanShape {
    let mut subject = shape(BillingCycle::Recurring);
    subject.frequency = Some(Frequency::Monthly);
    subject.rows = vec![
        recurring(0xb001, "usd", "US"),
        // A setup row makes EUR/EU a market the plan sells in, and it is not a
        // base row of the kind the cycle mandates.
        {
            let mut fee = record(
                0xb002,
                ChargeKind::OneTimeSetup,
                "eur",
                "EU",
                phase_id(TERMINAL),
            );
            fee.row.amount_minor = Some(minor(1_000));
            fee
        },
    ];
    subject
}

#[test]
fn a_recurring_plan_selling_where_it_has_no_base_row_is_refused_naming_the_market() {
    // `inst-cs-recurring` registered NOTHING before this rule: its other two
    // clauses are Slice 6's `billingTiming` and the frequency half of
    // `CYCLE_METADATA_MISSING`.
    let report = findings(&BaseMarketCompleteness, &recurring_missing_a_market());

    assert_eq!(codes(&report), vec![BASE_MARKET_INCOMPLETE]);
    let detail = details(&report);
    assert!(detail.contains("EUR"), "the market is named: {detail}");
    assert!(detail.contains("EU"), "{detail}");
    assert!(detail.contains("recurring"), "the kind is named: {detail}");
}

#[test]
fn a_one_time_plan_selling_where_it_has_no_base_row_is_refused_the_same_way() {
    // The other cardinality arm, and the reason it is one rule: a one-time plan
    // sold in a market with no `one_time` row is bought there for nothing.
    let mut subject = shape(BillingCycle::OneTime);
    subject.rows = vec![
        {
            let mut base = record(0xb001, ChargeKind::OneTime, "usd", "US", phase_id(TERMINAL));
            base.row.amount_minor = Some(minor(4_900));
            base
        },
        usage(0xb002, "eur", "EU", "api_calls", phase_id(TERMINAL)),
    ];

    let report = findings(&BaseMarketCompleteness, &subject);

    assert_eq!(codes(&report), vec![BASE_MARKET_INCOMPLETE]);
    assert!(
        details(&report).contains("one_time"),
        "{}",
        details(&report)
    );
}

#[test]
fn a_usage_only_plan_owes_no_base_row_at_all() {
    // The cycle that mandates none. A usage-only plan's market completeness is
    // `UsageMarketCompleteness`'s, over the metered lines - and a rule demanding
    // a base row here would be one no usage plan could ever satisfy.
    let mut subject = shape(BillingCycle::Usage);
    subject.rows = vec![usage(0xb001, "usd", "US", "api_calls", phase_id(TERMINAL))];

    assert!(
        findings(&BaseMarketCompleteness, &subject)
            .violations
            .is_empty()
    );
}

#[test]
fn every_market_short_of_a_base_row_is_reported_and_not_only_the_first() {
    // Section 9's enumerate-all contract: an author remediates in one pass.
    let mut subject = shape(BillingCycle::Recurring);
    subject.frequency = Some(Frequency::Monthly);
    subject.rows = vec![
        recurring(0xb001, "usd", "US"),
        usage(0xb002, "eur", "EU", "api_calls", phase_id(TERMINAL)),
        usage(0xb003, "gbp", "UK", "api_calls", phase_id(TERMINAL)),
    ];

    let report = findings(&BaseMarketCompleteness, &subject);

    assert_eq!(report.violations.len(), 2, "{:?}", details(&report));
    assert!(
        report
            .violations
            .iter()
            .all(|v| v.code == BASE_MARKET_INCOMPLETE)
    );
}

#[test]
fn the_base_market_rule_and_the_hybrid_rule_never_contest_one_plan() {
    // Two codes, two questions, and a plan must not get both for one remedy.
    // `HYBRID_INCOMPLETE` means a part is missing ANYWHERE and never names a
    // market; `BASE_MARKET_INCOMPLETE` means a part exists and does not reach
    // every sold market.

    // (a) A hybrid with no recurring part at all: the hybrid rule speaks, and
    //     the market rule defers.
    let mut no_part = shape(BillingCycle::Hybrid);
    no_part.frequency = Some(Frequency::Monthly);
    no_part.rows = vec![usage(0xb001, "usd", "US", "api_calls", phase_id(TERMINAL))];
    assert_eq!(
        codes(&findings(&HybridCompleteness, &no_part)),
        vec![HYBRID_INCOMPLETE]
    );
    assert!(
        findings(&BaseMarketCompleteness, &no_part)
            .violations
            .is_empty(),
        "the market rule defers: the remediation is 'add the recurring part', \
         not 'add it in four markets'"
    );

    // (b) A hybrid whose recurring part exists and misses a market: the market
    //     rule speaks, and the hybrid rule is satisfied.
    let mut short_market = shape(BillingCycle::Hybrid);
    short_market.frequency = Some(Frequency::Monthly);
    short_market.rows = vec![
        recurring(0xb001, "usd", "US"),
        usage(0xb002, "usd", "US", "api_calls", phase_id(TERMINAL)),
        usage(0xb003, "eur", "EU", "api_calls", phase_id(TERMINAL)),
    ];
    assert!(
        findings(&HybridCompleteness, &short_market)
            .violations
            .is_empty(),
        "both parts are present somewhere"
    );
    assert_eq!(
        codes(&findings(&BaseMarketCompleteness, &short_market)),
        vec![BASE_MARKET_INCOMPLETE]
    );
}

#[test]
fn a_non_hybrid_plan_with_no_base_row_anywhere_is_still_reported_here() {
    // The deferral is hybrid-only, because on any other cycle there is no hybrid
    // rule to defer to - and a plan with no base row at all would then be
    // reported by nothing.
    let mut subject = shape(BillingCycle::Recurring);
    subject.frequency = Some(Frequency::Monthly);
    subject.rows = vec![usage(0xb001, "usd", "US", "api_calls", phase_id(TERMINAL))];

    assert_eq!(
        codes(&findings(&BaseMarketCompleteness, &subject)),
        vec![BASE_MARKET_INCOMPLETE]
    );
}
