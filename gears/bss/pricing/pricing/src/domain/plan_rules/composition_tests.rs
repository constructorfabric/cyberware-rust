//! Tests for the plan-composition rules.
//!
//! Two families, and both are written against the *defect* rather than the
//! code. The injectivity family exists because D-103 restated the rule after
//! the old one was found to be enforced by nothing: so the cases pin what a
//! plan is now allowed to do (price several meters, repeat a line across
//! markets, phases, eligibility classes and grandfathered generations) as hard
//! as they pin what it may not (repeat a line inside one slice). The add-on
//! family pins the three ways a plan-authored edge set can be incoherent, and
//! one thing it must not do — fail a conflict pair that has an optional side,
//! which is the feature the field exists for.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{
    AddonConflictBothRequired, AddonDependencyAcyclic, AddonEdgeMembership, AddonQtyRange,
    MeterInjectivity, PLAN_NAME_MAX_CHARS, PlanNameWellFormed, PlanTierDeclared,
};
use crate::domain::concurrency::RowVersion;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::CurrencyCode;
use crate::domain::plan_rules::{
    ADDON_CYCLE, ADDON_INCOMPATIBLE, ADDON_QTY_RANGE_INVALID, METER_AMBIGUOUS, PLAN_NAME_INVALID,
    PLANTIER_MISSING,
};
use crate::domain::plan_shape::{AddonRule, PlanShape};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{ModelKind, PriceRow};
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::validation::Stage;
use crate::domain::validation::{ValidationReport, ValidationRule};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x91a4))
}

fn phase_id(seed: u128) -> PhaseId {
    PhaseId::new(Uuid::from_u128(seed))
}

/// The plan's terminal phase — where a phase-invariant usage row is filed
/// (D-19).
fn terminal_phase() -> PhaseId {
    phase_id(0x7e0)
}

fn addon(seed: u128) -> Uuid {
    Uuid::from_u128(seed)
}

fn now() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, 12, 0, 0)
}

fn cutover(day: u32) -> Cohort {
    Cohort::Generation(utc_ymd_hms(2026, 7, day, 0, 0, 0))
}

fn shape() -> PlanShape {
    let mut shape = PlanShape::new(plan(), 3, now());
    shape.plan_tier = Some("standard".to_owned());
    shape
}

/// One priced row, described by every axis this slice's rules read.
///
/// A struct rather than a wide constructor because the injectivity cases each
/// vary exactly one axis of a shared baseline, and a positional call would make
/// which one invisible at the call site.
#[derive(Clone)]
struct Line {
    price_id: u128,
    charge_kind: ChargeKind,
    currency: &'static str,
    region: &'static str,
    phase: PhaseId,
    eligibility: PriceEligibility,
    cohort: Cohort,
    meter: Option<&'static str>,
    dimension_key: &'static str,
}

impl Line {
    /// A metered row on `meter`, on the plan's terminal phase, in USD/EU.
    fn usage(price_id: u128, meter: &'static str) -> Self {
        Self {
            price_id,
            charge_kind: ChargeKind::Usage,
            currency: "USD",
            region: "EU",
            phase: terminal_phase(),
            eligibility: PriceEligibility::AllSubscriptions,
            cohort: Cohort::None,
            meter: Some(meter),
            dimension_key: "",
        }
    }

    /// A recurring row on the same slice, carrying no meter at all.
    fn unmetered(price_id: u128) -> Self {
        let mut line = Self::usage(price_id, "");
        line.charge_kind = ChargeKind::Recurring;
        line.meter = None;
        line
    }

    fn record(&self) -> PriceRecord {
        let scope_key = ScopeKey::new(
            plan(),
            CurrencyCode::new(self.currency).expect("test currency is three letters"),
            Region::new(self.region).expect("test region is non-blank"),
            self.phase,
            self.eligibility,
            self.charge_kind,
            self.cohort,
        )
        .expect("the test eligibility and cohort are paired");

        let mut row = PriceRow::new(self.charge_kind, Some(ModelKind::PerUnit));
        row.meter = self.meter.map(str::to_owned);
        row.dimension_key = self.dimension_key.to_owned();

        PriceRecord {
            price_id: Uuid::from_u128(self.price_id),
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
}

fn rule(required: bool, sku: Uuid) -> AddonRule {
    AddonRule {
        addon_sku_id: sku,
        required,
        min_qty: None,
        max_qty: Some(1),
        step_qty: None,
        price_override_ref: None,
        depends_on: Vec::new(),
        conflicts_with: Vec::new(),
    }
}

fn findings(rule: &impl ValidationRule<PlanShape>, subject: &PlanShape) -> ValidationReport {
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

/// What a report found, for the message of an assertion that expected nothing.
fn reported(report: &ValidationReport) -> String {
    codes(report).join(", ")
}

/// Run the injectivity rule over a row set built from `lines`.
fn injectivity(lines: &[Line]) -> ValidationReport {
    let mut subject = shape();
    subject.rows = lines.iter().map(Line::record).collect();
    findings(&MeterInjectivity, &subject)
}

/// Run the three add-on rules over `rules`, concatenating their findings the
/// way the pipeline does.
fn composition(rules: Vec<AddonRule>) -> ValidationReport {
    let mut subject = shape();
    subject.addon_rules = rules;
    let mut report = findings(&AddonEdgeMembership, &subject);
    report.absorb(findings(&AddonDependencyAcyclic, &subject));
    report.absorb(findings(&AddonConflictBothRequired, &subject));
    report
}

// ---------------------------------------------------------------------------
// inst-cmp-planname (D-318)
// ---------------------------------------------------------------------------

#[test]
fn an_unnamed_plan_is_silent() {
    // The column is nullable and the overwhelming majority of plans in any
    // catalog predate the name. Refusing absence would make every one of them
    // unpublishable, which is not what a label is worth.
    let mut subject = shape();
    subject.plan_name = None;

    assert!(findings(&PlanNameWellFormed, &subject).is_publishable());
}

#[test]
fn a_named_plan_is_silent() {
    let mut subject = shape();
    subject.plan_name = Some("Managed WordPress".to_owned());

    assert!(findings(&PlanNameWellFormed, &subject).is_publishable());
}

#[test]
fn an_empty_plan_name_is_refused_rather_than_read_as_unnamed() {
    // The whole reason the rule exists: `NULL` already means unnamed, and
    // storing `""` beside it gives one state two spellings. Every surface would
    // then have to treat them alike, and the first that forgot would render a
    // plan whose name is nothing at all.
    let mut subject = shape();
    subject.plan_name = Some(String::new());

    let report = findings(&PlanNameWellFormed, &subject);

    assert_eq!(codes(&report), vec![PLAN_NAME_INVALID]);
    assert_eq!(report.violations[0].subject, subject.subject());
    // **The stage, because the rule reports through `violate_at_write`.** Nothing
    // else in the crate asserts it: the REST door calls `plan_name_fault` directly
    // rather than filtering with `write_stage_only()`, so a stamp of `Publish` here
    // would leave this rule silent on the one door that runs it and no case would
    // see the change.
    assert_eq!(report.violations[0].stage, Stage::Write);
}

#[test]
fn a_whitespace_only_plan_name_is_refused_too() {
    // A space is not a name, and a `trim().is_empty()` check is the only thing
    // between "unnamed" and a row that looks named in the database and blank on
    // every screen.
    let mut subject = shape();
    subject.plan_name = Some(" \t ".to_owned());

    assert_eq!(
        codes(&findings(&PlanNameWellFormed, &subject)),
        vec![PLAN_NAME_INVALID]
    );
}

#[test]
fn a_plan_name_at_the_bound_is_silent_and_one_over_is_not() {
    // The pair, in one test: a bound tested only from the failing side passes
    // just as well when it is off by one in the permissive direction.
    let mut at_bound = shape();
    at_bound.plan_name = Some("n".repeat(PLAN_NAME_MAX_CHARS));
    assert!(findings(&PlanNameWellFormed, &at_bound).is_publishable());

    let mut over = shape();
    over.plan_name = Some("n".repeat(PLAN_NAME_MAX_CHARS + 1));
    assert_eq!(
        codes(&findings(&PlanNameWellFormed, &over)),
        vec![PLAN_NAME_INVALID]
    );
}

#[test]
fn the_bound_counts_characters_and_not_bytes() {
    // A name in a non-Latin script must get the same room as one in ASCII.
    // Counted in bytes, this 120-character name is 360 and would be refused.
    let mut subject = shape();
    // Escaped rather than literal: the workspace forbids non-ASCII in source
    // (`clippy::non_ascii_literal`), and a Cyrillic П is exactly the character
    // this case is about — three bytes, one character.
    subject.plan_name = Some("\u{041f}".repeat(PLAN_NAME_MAX_CHARS));

    assert!(findings(&PlanNameWellFormed, &subject).is_publishable());
}

// ---------------------------------------------------------------------------
// inst-cmp-plantier
// ---------------------------------------------------------------------------

#[test]
fn an_absent_plan_tier_fails_publish() {
    let mut subject = shape();
    subject.plan_tier = None;

    let report = findings(&PlanTierDeclared, &subject);

    assert_eq!(codes(&report), vec![PLANTIER_MISSING]);
    assert_eq!(report.violations[0].subject, subject.subject());
}

#[test]
fn a_blank_plan_tier_is_not_a_declaration() {
    // The equality-to-SKU half that would otherwise catch this is registry
    // work and absent, so a whitespace-only tier would publish unexamined.
    let mut subject = shape();
    subject.plan_tier = Some("   ".to_owned());

    assert_eq!(
        codes(&findings(&PlanTierDeclared, &subject)),
        vec![PLANTIER_MISSING]
    );
}

#[test]
fn a_declared_plan_tier_is_silent() {
    assert!(findings(&PlanTierDeclared, &shape()).is_publishable());
}

// ---------------------------------------------------------------------------
// inst-cmp-injective (D-103)
// ---------------------------------------------------------------------------

#[test]
fn two_rows_on_one_line_in_one_slice_are_ambiguous() {
    let first = Line::usage(0x1, "cloudlets");
    let second = Line::usage(0x2, "cloudlets");

    let report = injectivity(&[first, second]);

    assert_eq!(codes(&report), vec![METER_AMBIGUOUS]);
    let named = &report.violations[0].subject;
    assert!(named.contains("cloudlets"), "the line is named: {named}");
    assert!(named.contains("USD"), "the slice is named: {named}");
    assert!(named.contains("EU"), "the slice is named: {named}");
}

#[test]
fn a_plan_pricing_two_meters_publishes() {
    // The D-103 regression: the pre-restatement rule said a usage plan revision
    // maps exactly one meteringUnit, and a PaaS plan pricing cloudlets, storage
    // and egress is one plan rather than three.
    let report = injectivity(&[
        Line::usage(0x1, "cloudlets"),
        Line::usage(0x2, "storage"),
        Line::usage(0x3, "egress"),
    ]);

    assert!(report.is_publishable(), "unexpected: {}", reported(&report));
}

#[test]
fn one_meter_priced_over_two_dimensions_publishes() {
    let mut first = Line::usage(0x1, "egress");
    first.dimension_key = "eu-west";
    let mut second = Line::usage(0x2, "egress");
    second.dimension_key = "us-east";

    assert!(injectivity(&[first, second]).is_publishable());
}

#[test]
fn the_same_line_in_two_markets_publishes() {
    let mut other_currency = Line::usage(0x2, "cloudlets");
    other_currency.currency = "EUR";
    let mut other_region = Line::usage(0x3, "cloudlets");
    other_region.region = "US";

    let report = injectivity(&[Line::usage(0x1, "cloudlets"), other_currency, other_region]);

    assert!(report.is_publishable(), "unexpected: {}", reported(&report));
}

#[test]
fn the_same_line_in_two_phases_publishes() {
    let mut trial = Line::usage(0x2, "cloudlets");
    trial.phase = phase_id(0x7);

    assert!(injectivity(&[Line::usage(0x1, "cloudlets"), trial]).is_publishable());
}

#[test]
fn the_same_line_in_two_eligibility_classes_publishes() {
    let mut newcomers = Line::usage(0x2, "cloudlets");
    newcomers.eligibility = PriceEligibility::NewSubscriptionsOnly;

    assert!(injectivity(&[Line::usage(0x1, "cloudlets"), newcomers]).is_publishable());
}

#[test]
fn two_grandfathered_generations_of_one_line_publish() {
    // ADR-0002: the cohort axis is what lets a second cutover retain another
    // generation of the same usage line without reading as a duplicate.
    let mut july = Line::usage(0x2, "cloudlets");
    july.eligibility = PriceEligibility::ExistingGrandfathered;
    july.cohort = cutover(1);
    let mut august = Line::usage(0x3, "cloudlets");
    august.eligibility = PriceEligibility::ExistingGrandfathered;
    august.cohort = cutover(15);

    let report = injectivity(&[Line::usage(0x1, "cloudlets"), july, august]);

    assert!(report.is_publishable(), "unexpected: {}", reported(&report));
}

#[test]
fn two_rows_differing_only_in_charge_kind_still_collide() {
    // chargeKind is not a column of the index this rule restates, and the index
    // admits any row carrying a meter. A metered row mis-filed on a non-usage
    // charge component must therefore fail here rather than at the driver.
    let mut misfiled = Line::usage(0x2, "cloudlets");
    misfiled.charge_kind = ChargeKind::OneTimeSetup;

    assert_eq!(
        codes(&injectivity(&[Line::usage(0x1, "cloudlets"), misfiled])),
        vec![METER_AMBIGUOUS]
    );
}

#[test]
fn rows_without_a_meter_never_collide() {
    // NULL meters do not collide in the index, and two unmetered rows on one
    // slice are the ordinary shape of a plan, not an ambiguity.
    assert!(injectivity(&[Line::unmetered(0x1), Line::unmetered(0x2)]).is_publishable());
}

#[test]
fn a_third_row_on_a_doubled_line_is_reported_too() {
    let report = injectivity(&[
        Line::usage(0x1, "cloudlets"),
        Line::usage(0x2, "cloudlets"),
        Line::usage(0x3, "cloudlets"),
    ]);

    assert_eq!(codes(&report), vec![METER_AMBIGUOUS, METER_AMBIGUOUS]);
}

// ---------------------------------------------------------------------------
// inst-cmp-addons: edge membership
// ---------------------------------------------------------------------------

#[test]
fn a_dependency_on_a_non_member_fails() {
    let mut backup = rule(false, addon(0xa));
    backup.depends_on = vec![addon(0xff)];

    let report = composition(vec![backup, rule(false, addon(0xb))]);

    assert_eq!(codes(&report), vec![ADDON_INCOMPATIBLE]);
    assert_eq!(
        report.violations[0].subject,
        format!("{}/{}", addon(0xa), addon(0xff))
    );
}

#[test]
fn a_conflict_with_a_non_member_fails() {
    let mut backup = rule(false, addon(0xa));
    backup.conflicts_with = vec![addon(0xff)];

    assert_eq!(codes(&composition(vec![backup])), vec![ADDON_INCOMPATIBLE]);
}

#[test]
fn a_self_edge_fails() {
    let mut backup = rule(false, addon(0xa));
    backup.depends_on = vec![addon(0xa)];

    let report = composition(vec![backup]);

    // Exactly one finding: the self-edge is a loop of length one, and the cycle
    // walk deliberately leaves it to membership rather than reporting one
    // authoring mistake under two codes.
    assert_eq!(codes(&report), vec![ADDON_INCOMPATIBLE]);
}

#[test]
fn in_set_edges_are_silent() {
    let mut backup = rule(false, addon(0xa));
    backup.depends_on = vec![addon(0xb)];
    backup.conflicts_with = vec![addon(0xc)];

    let report = composition(vec![
        backup,
        rule(false, addon(0xb)),
        rule(false, addon(0xc)),
    ]);

    assert!(report.is_publishable(), "unexpected: {}", reported(&report));
}

// ---------------------------------------------------------------------------
// inst-cmp-addons: dependency cycles
// ---------------------------------------------------------------------------

#[test]
fn a_two_node_dependency_cycle_fails() {
    let mut first = rule(false, addon(0xa));
    first.depends_on = vec![addon(0xb)];
    let mut second = rule(false, addon(0xb));
    second.depends_on = vec![addon(0xa)];

    let report = composition(vec![first, second]);

    assert_eq!(codes(&report), vec![ADDON_CYCLE]);
    assert_eq!(
        report.violations[0].subject,
        format!("{},{}", addon(0xa), addon(0xb))
    );
}

#[test]
fn a_three_node_dependency_cycle_fails_with_stable_member_ordering() {
    // The case a pairwise check passes: no two of these depend on each other.
    // The members are authored out of order so the report cannot be a
    // restatement of authoring order.
    let mut third = rule(false, addon(0xc));
    third.depends_on = vec![addon(0xa)];
    let mut first = rule(false, addon(0xa));
    first.depends_on = vec![addon(0xb)];
    let mut second = rule(false, addon(0xb));
    second.depends_on = vec![addon(0xc)];

    let report = composition(vec![third, first, second]);

    assert_eq!(codes(&report), vec![ADDON_CYCLE]);
    assert_eq!(
        report.violations[0].subject,
        format!("{},{},{}", addon(0xa), addon(0xb), addon(0xc))
    );
}

#[test]
fn a_dependent_of_a_cycle_is_not_named_as_a_member() {
    // `d` reaches the loop and the loop does not reach it, so removing `d`
    // breaks nothing: naming it would send the author to the wrong rule.
    let mut first = rule(false, addon(0xa));
    first.depends_on = vec![addon(0xb)];
    let mut second = rule(false, addon(0xb));
    second.depends_on = vec![addon(0xa)];
    let mut dependent = rule(false, addon(0xd));
    dependent.depends_on = vec![addon(0xa)];

    let report = composition(vec![first, second, dependent]);

    assert_eq!(codes(&report), vec![ADDON_CYCLE]);
    assert_eq!(
        report.violations[0].subject,
        format!("{},{}", addon(0xa), addon(0xb))
    );
}

#[test]
fn two_loops_one_of_which_reaches_the_other_are_two_findings() {
    // `b` bridges into the second loop, so reachability alone folds all four
    // into one finding and names two add-ons that are not in the loop the
    // author has to break. Membership of a loop is mutual reachability, and
    // this is the shape where the two answers differ.
    let mut first = rule(false, addon(0xa));
    first.depends_on = vec![addon(0xb)];
    let mut bridge = rule(false, addon(0xb));
    bridge.depends_on = vec![addon(0xa), addon(0xc)];
    let mut third = rule(false, addon(0xc));
    third.depends_on = vec![addon(0xd)];
    let mut fourth = rule(false, addon(0xd));
    fourth.depends_on = vec![addon(0xc)];

    let report = composition(vec![first, bridge, third, fourth]);

    assert_eq!(codes(&report), vec![ADDON_CYCLE, ADDON_CYCLE]);
    assert_eq!(
        report.violations[0].subject,
        format!("{},{}", addon(0xa), addon(0xb))
    );
    assert_eq!(
        report.violations[1].subject,
        format!("{},{}", addon(0xc), addon(0xd))
    );
}

#[test]
fn a_dependency_diamond_publishes() {
    let mut top = rule(false, addon(0xa));
    top.depends_on = vec![addon(0xb), addon(0xc)];
    let mut left = rule(false, addon(0xb));
    left.depends_on = vec![addon(0xd)];
    let mut right = rule(false, addon(0xc));
    right.depends_on = vec![addon(0xd)];

    let report = composition(vec![top, left, right, rule(false, addon(0xd))]);

    assert!(report.is_publishable(), "unexpected: {}", reported(&report));
}

// ---------------------------------------------------------------------------
// inst-cmp-addons: conflicting pairs
// ---------------------------------------------------------------------------

#[test]
fn a_conflict_between_two_required_addons_fails() {
    let mut first = rule(true, addon(0xa));
    first.conflicts_with = vec![addon(0xb)];
    let mut second = rule(true, addon(0xb));
    second.conflicts_with = vec![addon(0xa)];

    // Authored high id first, so the subject pins the normalized pair rather
    // than restating whichever side the walk reached first.
    let report = composition(vec![second, first]);

    // Once, not twice: the pair is one fault however many sides authored it.
    assert_eq!(codes(&report), vec![ADDON_INCOMPATIBLE]);
    assert_eq!(
        report.violations[0].subject,
        format!("{},{}", addon(0xa), addon(0xb))
    );
}

#[test]
fn a_required_addon_conflicting_with_itself_is_reported_once() {
    // Membership owns the self-edge. Letting it through to the pair check as
    // well would report one authoring mistake under the same code twice.
    let mut lone = rule(true, addon(0xa));
    lone.conflicts_with = vec![addon(0xa)];

    assert_eq!(codes(&composition(vec![lone])), vec![ADDON_INCOMPATIBLE]);
}

#[test]
fn a_one_sided_conflict_between_two_required_addons_still_fails() {
    // Conflicts are stored normalized symmetric, so a constraint that bound
    // only its author would be one the other side could ignore.
    let mut first = rule(true, addon(0xa));
    first.conflicts_with = vec![addon(0xb)];

    let report = composition(vec![first, rule(true, addon(0xb))]);

    assert_eq!(codes(&report), vec![ADDON_INCOMPATIBLE]);
}

#[test]
fn a_conflict_with_one_optional_side_publishes() {
    // The feature the field exists for: a rule that failed every conflict pair
    // would forbid selection-time exclusivity outright.
    let mut required_side = rule(true, addon(0xa));
    required_side.conflicts_with = vec![addon(0xb)];

    let report = composition(vec![required_side, rule(false, addon(0xb))]);

    assert!(report.is_publishable(), "unexpected: {}", reported(&report));
}

#[test]
fn a_conflict_authored_by_the_optional_side_publishes_too() {
    // The same pair from the other end. Both sides have to be read before the
    // pair is judged, or which side happened to author the edge would decide
    // whether the plan publishes.
    let mut optional_side = rule(false, addon(0xa));
    optional_side.conflicts_with = vec![addon(0xb)];

    let report = composition(vec![optional_side, rule(true, addon(0xb))]);

    assert!(report.is_publishable(), "unexpected: {}", reported(&report));
}

#[test]
fn rule_names_are_the_instructions_that_own_them() {
    assert_eq!(PlanTierDeclared.name(), "inst-cmp-plantier");
    assert_eq!(MeterInjectivity.name(), "inst-cmp-injective");
    // One instruction states four separable properties of the add-on set, so the
    // three beside the membership rule name it and then the property. The suffix
    // is what lets `rule_names()` say which of the four is registered - the one
    // question the name answers, since a `Violation` carries a code and not a
    // rule name - the way `inst-ph-graph`'s three halves are told apart.
    assert_eq!(AddonEdgeMembership.name(), "inst-cmp-addons");
    assert_eq!(AddonDependencyAcyclic.name(), "inst-cmp-addons/acyclic");
    assert_eq!(AddonConflictBothRequired.name(), "inst-cmp-addons/conflict");
    assert_eq!(AddonQtyRange.name(), "inst-cmp-addons/qty");
}

// ---------------------------------------------------------------------------
// inst-cmp-addons, the quantity bounds (D-150)
// ---------------------------------------------------------------------------

/// One add-on rule with the three bounds set explicitly.
fn bounded(
    sku: Uuid,
    required: bool,
    min_qty: Option<u32>,
    max_qty: Option<u32>,
    step_qty: Option<u32>,
) -> AddonRule {
    AddonRule {
        min_qty,
        max_qty,
        step_qty,
        ..rule(required, sku)
    }
}

fn with_addons(rules: Vec<AddonRule>) -> PlanShape {
    let mut subject = PlanShape::new(plan(), 3, now());
    subject.addon_rules = rules;
    subject
}

#[test]
fn a_required_addon_nobody_may_take_any_of_is_refused_naming_the_bound() {
    let subject = with_addons(vec![bounded(addon(0xa1), true, None, Some(0), None)]);

    let report = findings(&AddonQtyRange, &subject);

    assert_eq!(codes(&report), vec![ADDON_QTY_RANGE_INVALID]);
    assert!(
        report.violations[0].detail.contains("maxQty"),
        "the offending bound is named: {}",
        report.violations[0].detail
    );
}

#[test]
fn a_required_addon_with_no_ceiling_at_all_is_refused_too() {
    // The sibling of the case above, and the one the rule used to miss: `maxQty`
    // ABSENT rather than zero. `is_some_and` reads an absent bound as satisfied, so
    // this fell through to `chk_pricing_plan_addon_rule_required_max_qty` and reached
    // the caller as a 500 naming a database constraint — the exact outcome this
    // rule's doc says it exists to replace with a report line.
    let subject = with_addons(vec![bounded(addon(0xa1), true, None, None, None)]);

    let report = findings(&AddonQtyRange, &subject);

    assert_eq!(codes(&report), vec![ADDON_QTY_RANGE_INVALID]);
    assert!(
        report.violations[0].detail.contains("maxQty is unset"),
        "the absent bound is named as absent, not as a number: {}",
        report.violations[0].detail
    );
}

#[test]
fn a_required_addon_with_a_real_ceiling_is_clean() {
    // The positive control. Without it the fix could refuse every required add-on
    // and both cases above would still pass.
    let subject = with_addons(vec![bounded(addon(0xa1), true, Some(1), Some(5), None)]);

    assert!(findings(&AddonQtyRange, &subject).violations.is_empty());
}

#[test]
fn an_empty_selection_window_is_refused_naming_both_bounds() {
    let subject = with_addons(vec![bounded(addon(0xa1), false, Some(5), Some(2), None)]);

    let report = findings(&AddonQtyRange, &subject);

    assert_eq!(codes(&report), vec![ADDON_QTY_RANGE_INVALID]);
    let detail = &report.violations[0].detail;
    assert!(detail.contains("minQty 5"), "{detail}");
    assert!(detail.contains("maxQty 2"), "{detail}");
}

#[test]
fn a_step_of_zero_is_refused() {
    let subject = with_addons(vec![bounded(addon(0xa1), false, None, Some(9), Some(0))]);

    let report = findings(&AddonQtyRange, &subject);

    assert_eq!(codes(&report), vec![ADDON_QTY_RANGE_INVALID]);
    assert!(report.violations[0].detail.contains("stepQty"));
}

#[test]
fn all_three_broken_bounds_on_one_addon_are_reported_and_not_only_the_first() {
    // Section 9's enumerate-all contract, and the whole reason D-150 exists: only
    // the first bound was ever written, and only as a section 6 CHECK with no
    // code - so an author met it as a driver error naming a constraint, one bound
    // at a time, and the other two were expressible at all.
    let subject = with_addons(vec![bounded(addon(0xa1), true, Some(4), Some(0), Some(0))]);

    let report = findings(&AddonQtyRange, &subject);

    assert_eq!(
        report.violations.len(),
        3,
        "three bounds, three lines: {:?}",
        report.violations
    );
    assert!(
        report
            .violations
            .iter()
            .all(|v| v.code == ADDON_QTY_RANGE_INVALID)
    );
    assert!(
        report
            .violations
            .iter()
            .all(|v| v.subject.contains(&addon(0xa1).to_string())),
        "each names the add-on it is about"
    );
}

#[test]
fn a_plan_whose_addon_bounds_admit_a_selection_is_silent() {
    // Every arm's negative side. An optional add-on with a zero `maxQty` is
    // legal - nothing is mandated - and an unset bound is not a bound of zero
    // **except on a required add-on's `maxQty`**, where absent and zero fail
    // alike: `chk_pricing_plan_addon_rule_required_max_qty` is
    // `required => max_qty IS NOT NULL AND max_qty >= 1`. This list used to carry
    // `(required, None, None, None)` as a silent case, which is what let the rule
    // ship reading an absent ceiling as satisfied; that row is now its own test,
    // and it is refused.
    for rules in [
        vec![bounded(addon(0xa1), true, Some(1), Some(3), Some(1))],
        vec![bounded(addon(0xa1), false, None, Some(0), None)],
        vec![bounded(addon(0xa1), false, None, None, None)],
        vec![bounded(addon(0xa1), false, Some(2), Some(2), None)],
    ] {
        let report = findings(&AddonQtyRange, &with_addons(rules));
        assert!(report.violations.is_empty(), "{}", reported(&report));
    }
}
