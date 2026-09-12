//! What the aggregate run promises: one report, every finding, the same answer
//! twice.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{
    GRANDFATHER_UNTIL_FORBIDDEN, PLAN_SIZE_SOFT_CAP_EXCEEDED, PRIMITIVE_RULES_UNBUILT,
    PublishRuleParams, ROUNDING_POLICY_UNRESOLVED, ReferencingMarket, SoftSizeCaps,
    run_publish_rules,
};
use crate::domain::bundle_rules::BUNDLE_TAX_BASIS_MIXED;
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{BillingAnchorPolicy, ProrationBasis, ProrationContract};
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount, RateMinor};
use crate::domain::plan_rules::{
    CustomIntervalBounds, DescriptorSetComplete, HYBRID_INCOMPLETE, INVALID_CUSTOM_INTERVAL,
    PHASE_GRAPH_INVALID, PHASE_ROW_ORPHANED, PLANTIER_MISSING,
};
use crate::domain::plan_shape::{BillingCycle, Frequency, PlanShape};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{ModelKind, PriceRow};
use crate::domain::rules::MODEL_KIND_MISSING;
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::tax_display::{RegionReadiness, RegionTaxReadiness, TaxDisplayPolicy};
use crate::domain::taxonomy::REGION_UNKNOWN;
use crate::domain::taxonomy::{GL_CODE_UNKNOWN, ROUNDING_POLICY_UNKNOWN};
use crate::domain::window::{KeyWindows, WindowInterval, WindowState};
use time::OffsetDateTime;

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x91a4))
}

fn now() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, 12, 0, 0)
}

fn params(default_rounding_policy: Option<&str>) -> PublishRuleParams {
    // **Declares `eu`**, which is the region every fixture row in this file sells
    // in. Not decoration: `inst-tx-region` is registered in the Foundation set,
    // and the empty universe is C2's fail-closed reading — a tenant who has
    // declared no region publishes nothing. So a `params` that declared none
    // would make every case in this file fail on a region it is not about, which
    // is exactly what happened the moment the rule was registered.
    //
    // `params_declaring` is what the three cases that are *about* the universe
    // use.
    base_params(default_rounding_policy)
        .with_declared_regions(declared(&["eu"]))
        // And `eu`'s readiness, for the same reason it declares `eu` at all:
        // `inst-td-policy` is registered in the Foundation set and C4 is
        // fail-closed, so a fixture whose region declares no tax category would
        // fail every case in this file on a rule none of them is about.
        .with_tax_display(TaxDisplayPolicy::FailClosed, readiness_for(&["eu"]))
}

/// A readiness lookup declaring a category and a rate for each named region.
fn readiness_for(regions: &[&str]) -> RegionTaxReadiness {
    RegionTaxReadiness::new(
        regions
            .iter()
            .map(|r| {
                (
                    (*r).to_owned(),
                    RegionReadiness {
                        tax_category: Some("standard".to_owned()),
                        tax_rate_present: true,
                    },
                )
            })
            .collect(),
    )
}

/// A region universe, as a set.
fn declared(regions: &[&str]) -> std::collections::BTreeSet<Region> {
    regions
        .iter()
        .map(|r| Region::new(r).expect("a non-blank fixture region"))
        .collect()
}

/// The base parameters, before a region universe is attached.
fn base_params(default_rounding_policy: Option<&str>) -> PublishRuleParams {
    PublishRuleParams::new(
        // The ratified launch caps, not zeros: a zero cap rejects every custom
        // frequency ever authored and would exercise that state instead.
        CustomIntervalBounds::new(366, 24),
        DescriptorSetComplete::default(),
        default_rounding_policy.map(ToOwned::to_owned),
        // Likewise the ratified soft caps (100 bands/row, 500 rows/plan) rather
        // than zeros, which would advise on every plan in this file.
        SoftSizeCaps::new(100, 500),
    )
}

fn record(price_id: u128, model_kind: Option<ModelKind>, rounding: Option<&str>) -> PriceRecord {
    let scope_key = ScopeKey::new(
        plan(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("non-blank"),
        PhaseId::new(Uuid::from_u128(0xf1)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none");

    let mut row = PriceRow::new(ChargeKind::Recurring, model_kind);
    row.amount_minor = Some(MinorAmount::new(1000).expect("non-negative"));

    PriceRecord {
        price_id: Uuid::from_u128(price_id),
        scope_key,
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        // Stated, because this is a **recurring** row and Slice 6's
        // `inst-bt-required` makes the field mandatory on one. Every test in
        // this file that asserts an *empty* report needs a row that is
        // publishable in every respect but the one under judgement, and a row
        // whose `billingTiming` is absent is not — the fixture was a recurring
        // row no publish would have accepted, which nothing measured while the
        // rule had no code.
        billing_timing: Some("advance".to_owned()),
        // Stated, because this is a **recurring** row and Slice 6's
        // `inst-pi-required` makes the three proration inputs mandatory on one.
        // A fixture that asserts a clean publish needs a row publishable in every
        // respect but the one under judgement, and a row with no proration
        // contract is not.
        proration_contract: Some(ProrationContract {
            billing_anchor_policy: BillingAnchorPolicy::CalendarMonth,
            proration_basis: ProrationBasis::CalendarDaysActual,
            credit_on_downgrade: false,
        }),
        rounding_policy_ref: rounding.map(ToOwned::to_owned),
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Draft,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: now(),
        row_version: RowVersion::new(0),
    }
}

fn codes(report: &crate::domain::validation::ValidationReport) -> Vec<String> {
    report
        .violations
        .iter()
        .map(|violation| violation.code.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// The aggregate run.
// ---------------------------------------------------------------------------

#[test]
fn a_row_shape_fault_and_a_plan_shape_fault_appear_in_one_report_in_the_fixed_order() {
    // The two halves of the contract at once: the report is aggregate, and the
    // row findings come first because a malformed row is malformed regardless
    // of the plan it sits in.
    let mut shape = PlanShape::new(plan(), 1, now());
    shape.billing_cycle = Some(BillingCycle::Recurring);
    // `inst-cs-declared` (D-149): a plan that recurs owes a
    // frequency. These fixtures used to omit it and pass, which is
    // the vacuous pass that rule exists to close.
    shape.frequency = Some(Frequency::Monthly);
    // No model kind: a Slice-3 row fault.
    shape.rows = vec![record(0xb001, None, Some("half_up"))];
    // No plan tier, no phases, no descriptor set: Slice-2 plan faults. The absent
    // phase set now costs a second finding rather than one — the row keys on a
    // phase nothing attached (`PHASE_ROW_ORPHANED`, D-337) — which is why the two
    // assertions below read the report's **ends** and not its length.

    let report = run_publish_rules(&shape, &params(Some("half_up")));
    let found = codes(&report);

    assert!(!report.is_publishable());
    assert_eq!(
        found.first().map(String::as_str),
        Some(MODEL_KIND_MISSING),
        "the row half of the report comes first"
    );
    // And the slice-7 coverage set comes **last**, which `rules.rs`'s module doc
    // argues for and nothing asserted: a report naming an uncovered key before it
    // names the row that is not a row reads back to front. This fixture authors no
    // window, so the coverage set has a finding of its own to place — which is what
    // makes the position assertable at all rather than vacuous.
    assert_eq!(
        found.last().map(String::as_str),
        Some(crate::domain::coverage::WINDOW_COVERAGE_MISSING),
        "the window plane is a statement about a set of rows, so it reads last"
    );
    assert!(found.iter().any(|code| code == PLANTIER_MISSING));
    assert!(found.iter().any(|code| code == PHASE_GRAPH_INVALID));
}

#[test]
fn one_awful_plan_produces_every_expected_violation_rather_than_the_first() {
    // The fail-closed contract's whole point: an author who submits a plan that
    // is wrong in several ways is told about all of them and remediates in one
    // pass. A run that stopped at the first would still block the publish and
    // would still look correct from outside.
    let mut shape = PlanShape::new(plan(), 1, now());
    shape.billing_cycle = Some(BillingCycle::Hybrid);
    // `inst-cs-declared` (D-149): a plan that recurs owes a
    // frequency. These fixtures used to omit it and pass, which is
    // the vacuous pass that rule exists to close.
    shape.frequency = Some(Frequency::Monthly);
    shape.rows = vec![record(0xb001, None, None), record(0xb002, None, None)];

    let report = run_publish_rules(&shape, &params(None));
    let found = codes(&report);

    for expected in [
        MODEL_KIND_MISSING,
        ROUNDING_POLICY_UNRESOLVED,
        HYBRID_INCOMPLETE,
        PLANTIER_MISSING,
        PHASE_GRAPH_INVALID,
        // D-337's, and the **sixth** distinct code this plan produces: the fixture
        // attaches no phase at all and both its rows key on one, so each is filed
        // under a phase the revision does not hold — twice over. It was carrying
        // that fault before the rule existed, which is what the rule is for and
        // not a defect in a fixture whose purpose is to be wrong in every way at
        // once. The roster gains the code rather than the fixture being repaired:
        // repairing it means attaching a phase, and `PHASE_GRAPH_INVALID` above is
        // what this plan is here to produce.
        PHASE_ROW_ORPHANED,
        // Slice 7's, and the seventh this plan produces: both rows sit on one
        // canonical scope key and it holds no window, so `inst-wc-required` names
        // it once. The roster was five while the report was six, which `any(...)`
        // tolerates in silence — the whole failure mode this test is named for.
        crate::domain::coverage::WINDOW_COVERAGE_MISSING,
    ] {
        assert!(
            found.iter().any(|code| code == expected),
            "{expected} must be in the aggregate report; got {found:?}"
        );
    }
    // Both rows are reported, not just the first one found.
    assert_eq!(
        found
            .iter()
            .filter(|code| *code == MODEL_KIND_MISSING)
            .count(),
        2
    );
}

#[test]
fn a_clean_plan_yields_an_empty_report() {
    let report = run_publish_rules(&clean_plan(), &params(Some("half_up")));

    assert_eq!(codes(&report), Vec::<String>::new());
    assert!(report.is_publishable());
}

/// The aggregate **runs** Slice 7's coverage set, and that is a fact of its own.
///
/// `domain/coverage_tests.rs` proves what each coverage rule refuses; this proves
/// that `run_publish_rules` — the function §4.2 step 2 and step 4 both call — is
/// wired to them. Without it the rules would be a pipeline nothing runs, which is
/// exactly the state the whole of this module was written to end.
///
/// The subject is `clean_plan()` with its window removed and nothing else changed,
/// so the only thing that can produce a finding is the set under test.
///
/// # What removing the registration actually reddens, corrected
///
/// The commit that landed this rule claimed the `report.absorb(window_coverage_rules()…)`
/// line reddens *"exactly that test and nothing else, measured across the whole
/// suite"*, and an earlier version of this doc said the same in the other
/// direction — that deleting it *"would leave every coverage assertion in the crate
/// green"*. **Both were false, and the property is stronger than they claimed.**
/// Removing the line reddens **four**:
///
/// - this test, on the aggregate report being empty;
/// - `a_row_shape_fault_and_a_plan_shape_fault_appear_in_one_report_in_the_fixed_order`,
///   on the coverage finding no longer being last;
/// - `one_awful_plan_produces_every_expected_violation_rather_than_the_first`, on
///   the sixth code being absent from the report;
/// - and `rest_windows::the_report_names_the_uncovered_key_the_publish_refusal_named`
///   — **the second independent guard, and the one worth naming**, because it holds
///   the property *across the router*: it takes the refused key off a real `POST
///   …/publish` response and the reported key off a real `GET …/coverage`, and
///   asserts the two strings are equal. An in-process registration test cannot
///   witness that the refusal reaches the wire at all.
///
/// The **second** direction of the proof is unchanged and does hold: not one of the
/// seventy-four fixture-churn tests reddens, which is what says the churn was this
/// rule and not something else the fixtures happened to be missing.
#[test]
fn the_aggregate_runs_the_slice_seven_coverage_set() {
    let mut uncovered = clean_plan();
    uncovered.windows = Vec::new();

    let report = run_publish_rules(&uncovered, &params(Some("half_up")));

    assert_eq!(
        codes(&report),
        [crate::domain::coverage::WINDOW_COVERAGE_MISSING],
        "the aggregate carries the coverage set, and the covered fixture proves the finding is the window's absence and not the plan's"
    );
}

/// The aggregate carries Slice 6's set, and the clean fixture proves the
/// finding is the absent field and not something else the plan is missing.
#[test]
fn the_aggregate_runs_the_slice_six_consumer_contract_set() {
    let mut untimed = clean_plan();
    for record in &mut untimed.rows {
        record.billing_timing = None;
    }

    let report = run_publish_rules(&untimed, &params(Some("half_up")));

    assert_eq!(
        codes(&report),
        [crate::domain::contracts::BILLING_TIMING_MISSING]
    );
}

#[test]
fn the_same_subject_run_twice_yields_byte_identical_reports() {
    // The property §4.2's two runs depend on, and the reason no rule may hold
    // state: approval approves content and the commit re-validates state, and
    // the two are only comparable if the rule set is a pure function of what it
    // is handed.
    let shape = clean_plan();
    let params = params(None);

    assert_eq!(
        run_publish_rules(&shape, &params),
        run_publish_rules(&shape, &params)
    );
}

// ---------------------------------------------------------------------------
// The Foundation's own rounding rule.
// ---------------------------------------------------------------------------

#[test]
fn a_row_with_neither_its_own_policy_nor_a_tenant_default_is_unpublishable() {
    let mut shape = clean_plan();
    shape.rows = vec![record(0xb001, Some(ModelKind::Flat), None)];

    let report = run_publish_rules(&shape, &params(None));

    assert_eq!(codes(&report), [ROUNDING_POLICY_UNRESOLVED]);
    assert_eq!(
        report.violations[0].subject,
        Uuid::from_u128(0xb001).to_string(),
        "the finding names the row the author has to edit"
    );
}

#[test]
fn a_row_carrying_its_own_policy_resolves() {
    let mut shape = clean_plan();
    shape.rows = vec![record(0xb001, Some(ModelKind::Flat), Some("half_up"))];

    assert!(run_publish_rules(&shape, &params(None)).is_publishable());
}

#[test]
fn a_tenant_default_resolves_every_row_at_once() {
    let mut shape = clean_plan();
    shape.rows = vec![
        record(0xb001, Some(ModelKind::Flat), None),
        record(0xb002, Some(ModelKind::Flat), None),
    ];

    assert!(run_publish_rules(&shape, &params(Some("bankers"))).is_publishable());
}

#[test]
fn a_blank_tenant_default_is_no_default_and_the_rows_are_still_judged() {
    // The guard lifts the per-row resolution check off the **whole plan**, so what it
    // admits it admits everywhere. A default of `"   "` names no policy: rounding
    // decides the last minor unit of every charge on these rows and nothing would
    // have chosen it. The column carries no non-blank CHECK, so this value reaches
    // the rule from a real store rather than only from a test.
    let mut shape = clean_plan();
    shape.rows = vec![
        record(0xb101, Some(ModelKind::Flat), None),
        record(0xb102, Some(ModelKind::Flat), None),
    ];

    for blank in ["", "   "] {
        let report = run_publish_rules(&shape, &params(Some(blank)));
        assert_eq!(
            codes(&report),
            [ROUNDING_POLICY_UNRESOLVED, ROUNDING_POLICY_UNRESOLVED],
            "a default of {blank:?} resolves nothing, so both rows are unresolved"
        );
    }
}

/// The descriptor's `glCode` is judged against the declared GL-code vocabulary
/// at publish, and **only** once one is declared (D-356).
///
/// Three runs over one clean plan, because each alone is satisfied by a wrong
/// registration: a rule that refused every code passes the first, a rule nobody
/// registered passes the second and third, and a rule that treated the empty set
/// as "nothing is declared, so nothing is allowed" passes the first two and fails
/// every existing catalog on landing.
#[test]
fn a_gl_code_outside_the_declared_vocabulary_is_refused_at_publish_and_only_then() {
    let shape = clean_plan();
    // `clean_plan` authors `4000`.
    assert_eq!(
        shape
            .descriptor_set
            .as_ref()
            .and_then(|set| set.gl_code.as_deref()),
        Some("4000"),
        "the fixture premise this case reads"
    );

    let refused = run_publish_rules(
        &shape,
        &params(Some("half_up"))
            .with_declared_gl_codes(declared_policies(&["4000-REV", "4010-TAX"])),
    );
    assert_eq!(codes(&refused), [GL_CODE_UNKNOWN]);

    let declared = run_publish_rules(
        &shape,
        &params(Some("half_up")).with_declared_gl_codes(declared_policies(&["4000", "4010-TAX"])),
    );
    assert!(
        declared.is_publishable(),
        "the declared code clears the rule: {:?}",
        codes(&declared)
    );

    let unconstrained = run_publish_rules(&shape, &params(Some("half_up")));
    assert!(
        unconstrained.is_publishable(),
        "a tenant that declared no GL codes has not opted in: {:?}",
        codes(&unconstrained)
    );
}

#[test]
fn a_tenant_default_outside_the_declared_vocabulary_is_refused() {
    // The default escaped **both** rules at once, which is why nothing caught it.
    //
    // `RoundingPolicyResolved` opens with `if self.tenant_default.is_some()
    // { return; }`, so merely *having* a default lifts the resolution check off
    // every row in the plan. `RoundingPolicyDeclared` then walks the rows and
    // `continue`s on every `rounding_policy_ref: None` - which is precisely the
    // set the default stands in for. So a tenant whose default names a value
    // D-334's vocabulary does not contain published every plan, and the retirement
    // path meanwhile counts that same default as a live reference and refuses to
    // retire the value it names.
    let mut shape = clean_plan();
    shape.rows = vec![
        record(0xb001, Some(ModelKind::Flat), None),
        record(0xb002, Some(ModelKind::Flat), None),
    ];

    let report = run_publish_rules(
        &shape,
        &params(Some("made_up"))
            .with_declared_rounding_policies(declared_policies(&["half_up", "bankers"])),
    );

    // One finding, not one per row: the rule already dedupes by reference, and
    // here the remediation is a single tenant-level act rather than N row edits.
    assert_eq!(codes(&report), [ROUNDING_POLICY_UNKNOWN]);
    assert!(
        report.violations[0].detail.contains("default"),
        "the finding has to say the reference came from the tenant default and not from a \
         row, or it sends the operator to edit rows that name nothing: {}",
        report.violations[0].detail
    );
}

#[test]
fn a_tenant_default_inside_the_declared_vocabulary_publishes() {
    // The positive control the refusal above is worthless without: the check must
    // reject the value, not the presence of a default.
    let mut shape = clean_plan();
    shape.rows = vec![record(0xb001, Some(ModelKind::Flat), None)];

    let report = run_publish_rules(
        &shape,
        &params(Some("bankers"))
            .with_declared_rounding_policies(declared_policies(&["half_up", "bankers"])),
    );

    assert!(report.is_publishable(), "{:?}", codes(&report));
}

#[test]
fn a_row_carrying_its_own_policy_does_not_lean_on_an_unknown_default() {
    // The narrowing: the default is only judged where a row actually relies on
    // it. A plan whose every row names its own policy is not refused for a
    // tenant-level setting it never reads.
    let mut shape = clean_plan();
    shape.rows = vec![record(0xb001, Some(ModelKind::Flat), Some("half_up"))];

    let report = run_publish_rules(
        &shape,
        &params(Some("made_up"))
            .with_declared_rounding_policies(declared_policies(&["half_up", "bankers"])),
    );

    assert!(report.is_publishable(), "{:?}", codes(&report));
}

/// A rounding-policy vocabulary, as a set.
fn declared_policies(values: &[&str]) -> std::collections::BTreeSet<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

#[test]
fn every_unresolved_row_is_reported_and_not_only_the_first() {
    let mut shape = clean_plan();
    shape.rows = vec![
        record(0xb001, Some(ModelKind::Flat), None),
        record(0xb002, Some(ModelKind::Flat), Some("half_up")),
        record(0xb003, Some(ModelKind::Flat), None),
    ];

    let report = run_publish_rules(&shape, &params(None));

    assert_eq!(
        codes(&report),
        [ROUNDING_POLICY_UNRESOLVED, ROUNDING_POLICY_UNRESOLVED]
    );
}

#[test]
fn the_rounding_code_is_spelled_as_the_design_set_spells_it() {
    // Transcribed from `01-foundation.md` §3.3, not from the identifier: a
    // constant whose value drifted from its name would still compile and would
    // still be wrong on the wire.
    assert_eq!(ROUNDING_POLICY_UNRESOLVED, "ROUNDING_POLICY_UNRESOLVED");
}

// ---------------------------------------------------------------------------
// `inst-tx-region` — region membership, registered in the Foundation set.
// ---------------------------------------------------------------------------

/// A candidate row whose region the tenant never declared fails publish.
///
/// The rule itself is `domain::taxonomy`'s and is unit-tested there. What this
/// case establishes is the thing that was missing: that it is **registered**, so
/// a real publish evaluates it. Before this, `RegionsDeclared` existed, was
/// tested, and ran on nothing — a plan carrying an undeclared region published
/// clean and `REGION_UNKNOWN` could not reach the wire.
#[test]
fn a_row_in_an_undeclared_region_fails_the_registered_set() {
    let shape = clean_plan();
    // `clean_plan`'s row sells in `eu`; the tenant declares only `us`.
    let report = run_publish_rules(&shape, &params_declaring(&["us"]));

    assert!(!report.is_publishable());
    assert!(
        codes(&report).contains(&REGION_UNKNOWN.to_owned()),
        "the registered set must raise the price-row refusal: {:?}",
        codes(&report)
    );
}

/// The same plan passes once its region is declared.
///
/// The positive half, and it is not decoration: a rule that refused every plan
/// would satisfy the case above while making the gear unusable, which is this
/// program's standing warning about a refusal test with no positive control.
#[test]
fn the_same_plan_publishes_once_its_region_is_declared() {
    let shape = clean_plan();

    let report = run_publish_rules(&shape, &params_declaring(&["eu"]));

    assert_eq!(codes(&report), Vec::<String>::new());
    assert!(report.is_publishable());
}

/// A tenant that has declared no region at all fails closed, not open.
///
/// C2 carves out no tenant. The opposite reading — an empty universe permits
/// everything — is the shape D-211 was written against, and it is the difference
/// between a fail-closed rule and one that is off until somebody configures it on.
#[test]
fn a_tenant_with_no_declared_region_fails_closed_at_publish() {
    let shape = clean_plan();

    let report = run_publish_rules(&shape, &params_declaring(&[]));

    assert!(
        codes(&report).contains(&REGION_UNKNOWN.to_owned()),
        "an unconfigured tenant publishes nothing: {:?}",
        codes(&report)
    );
}

/// The base parameters with **only** the named regions declared.
///
/// Off [`base_params`] rather than [`params`], which already declares `eu`:
/// building on that one would make `params_declaring(&[])` declare `eu` anyway
/// and the fail-closed case would silently test nothing.
fn params_declaring(regions: &[&str]) -> PublishRuleParams {
    base_params(Some("half_up"))
        .with_declared_regions(declared(regions))
        .with_tax_display(TaxDisplayPolicy::FailClosed, readiness_for(regions))
}

// ---------------------------------------------------------------------------
// A plan the whole set passes, so the tests above vary one thing at a time.
// ---------------------------------------------------------------------------

fn clean_plan() -> PlanShape {
    use crate::domain::plan_shape::{DescriptorSet, PhaseGraph, PhaseKind, PlanPhase};

    let terminal = PhaseId::new(Uuid::from_u128(0xf1));
    let mut shape = PlanShape::new(plan(), 1, now());
    shape.billing_cycle = Some(BillingCycle::Recurring);
    // `inst-cs-declared` (D-149): a plan that recurs owes a
    // frequency. These fixtures used to omit it and pass, which is
    // the vacuous pass that rule exists to close.
    shape.frequency = Some(Frequency::Monthly);
    shape.plan_tier = Some("standard".to_owned());
    shape.phases = PhaseGraph::new(vec![PlanPhase {
        phase_id: terminal,
        kind: PhaseKind::Evergreen,
        display_name: None,
        ordinal: 0,
        converts_to_phase_id: None,
        phase_duration_days: None,
        display_trial_days: None,
    }]);
    shape.descriptor_set = Some(DescriptorSet {
        invoice_line_template: Some("{plan}".to_owned()),
        gl_code: Some("4000".to_owned()),
        itemization_rule: Some("per_charge".to_owned()),
        additional: std::collections::BTreeMap::new(),
    });
    shape.rows = vec![record(0xb001, Some(ModelKind::Flat), Some("half_up"))];
    // `inst-wc-required`: a billable row whose canonical scope key holds no
    // active or scheduled window fails publish, so a plan the whole set passes
    // has to say when its row is effective. Seven fixtures in this file published
    // with no window at all until the rule registered, which is the same vacuous
    // pass `inst-cs-declared` closed two lines above — a fixture that satisfies a
    // rule by not reaching it.
    //
    // Open-ended, because nothing in this file is about coverage *ending*: the
    // interval set has to be sound and this is the soundest one there is. The
    // cases that vary it live in `domain/coverage_tests.rs`.
    shape.windows = vec![KeyWindows {
        scope_key: shape.rows[0].scope_key.clone(),
        intervals: vec![WindowInterval::new(now(), None, WindowState::Scheduled)],
    }];
    shape
}

// ---------------------------------------------------------------------------
// nfr-size-limits — the soft caps, advisory (D-160)
// ---------------------------------------------------------------------------

/// `params`, with the two soft caps set to whatever a case needs.
fn params_capped(bands: u32, rows: u32) -> PublishRuleParams {
    // Declares `eu` for [`params`]' reason: these cases are about the soft caps,
    // and a plan refused for its region would report a violation beside the
    // advisory they assert on.
    PublishRuleParams::new(
        CustomIntervalBounds::new(366, 24),
        DescriptorSetComplete::default(),
        Some("half_up".to_owned()),
        SoftSizeCaps::new(bands, rows),
    )
    .with_declared_regions(declared(&["eu"]))
    .with_tax_display(TaxDisplayPolicy::FailClosed, readiness_for(&["eu"]))
}

fn advisories(report: &crate::domain::validation::ValidationReport) -> Vec<String> {
    report
        .warnings
        .iter()
        .map(|advisory| advisory.code.clone())
        .collect()
}

#[test]
fn a_plan_above_the_row_cap_publishes_with_an_advisory_naming_the_cap_and_the_count() {
    // PRD section 14 ratifies both caps as a SHOULD emitting a publish warning.
    // Making them blocking so they could reuse a violation code is D-160's
    // rejected option (b) - and it would turn a raised-then-lowered cap into a
    // published plan its own tenant cannot re-publish.
    let mut shape = clean_plan();
    shape.rows = (0..3)
        .map(|n| record(0xb100 + n, Some(ModelKind::Flat), Some("half_up")))
        .collect();

    let report = run_publish_rules(&shape, &params_capped(100, 2));

    assert!(report.is_publishable(), "an advisory never blocks");
    assert_eq!(codes(&report), Vec::<String>::new());
    assert_eq!(advisories(&report), vec![PLAN_SIZE_SOFT_CAP_EXCEEDED]);
    let detail = &report.warnings[0].detail;
    assert!(detail.contains('3'), "the count is named: {detail}");
    assert!(detail.contains('2'), "and the limit: {detail}");
}

#[test]
fn a_row_above_the_band_cap_does_the_same_and_names_the_row() {
    let mut shape = clean_plan();
    let mut row = record(0xb200, Some(ModelKind::Graduated), Some("half_up"));
    row.row.bands = vec![
        crate::domain::price_row::TierBand::closed(
            0,
            10,
            RateMinor::from_nano_minor(500_000_000_000).expect("a non-negative rate"),
        ),
        crate::domain::price_row::TierBand::closed(
            10,
            20,
            RateMinor::from_nano_minor(400_000_000_000).expect("a non-negative rate"),
        ),
        crate::domain::price_row::TierBand::open(
            20,
            RateMinor::from_nano_minor(300_000_000_000).expect("a non-negative rate"),
        ),
    ];
    shape.rows = vec![row];

    let report = run_publish_rules(&shape, &params_capped(2, 500));

    // The row's own Slice-3 shape is not this test's subject - what matters is
    // that the size finding is an advisory and not among the violations.
    assert!(
        !codes(&report).contains(&PLAN_SIZE_SOFT_CAP_EXCEEDED.to_owned()),
        "the cap never blocks: {:?}",
        codes(&report)
    );
    assert_eq!(advisories(&report), vec![PLAN_SIZE_SOFT_CAP_EXCEEDED]);
    assert!(
        report.warnings[0]
            .subject
            .contains(&Uuid::from_u128(0xb200).to_string()),
        "the row is named: {}",
        report.warnings[0].subject
    );
}

#[test]
fn a_tenant_with_a_raised_cap_gets_no_advisory_at_the_same_size() {
    // The per-tenant read, and what D-152 bought: the value in force is the
    // authoring tenant's, so the same plan advises under one policy and not
    // under another. A rule that read the deployment default would make the
    // per-tenant carrier unobservable.
    let mut shape = clean_plan();
    shape.rows = (0..3)
        .map(|n| record(0xb100 + n, Some(ModelKind::Flat), Some("half_up")))
        .collect();

    assert_eq!(
        advisories(&run_publish_rules(&shape, &params_capped(100, 2))),
        vec![PLAN_SIZE_SOFT_CAP_EXCEEDED]
    );
    assert!(
        advisories(&run_publish_rules(&shape, &params_capped(100, 500))).is_empty(),
        "a raised cap silences it at the same size"
    );
}

#[test]
fn a_plan_at_the_cap_exactly_is_not_above_it() {
    // Both boundaries, on the side a `>` and a `>=` differ: the cap is a limit
    // the plan may reach.
    let mut shape = clean_plan();
    shape.rows = (0..2)
        .map(|n| record(0xb100 + n, Some(ModelKind::Flat), Some("half_up")))
        .collect();

    assert!(advisories(&run_publish_rules(&shape, &params_capped(100, 2))).is_empty());
}

#[test]
fn the_hard_interval_caps_still_block_and_produce_no_advisory() {
    // Section 1.2's allocation row had blurred all four caps under one heading and
    // D-160 unblurs them: the two interval caps are HARD and keep
    // `INVALID_CUSTOM_INTERVAL`. A soft advisory reaching them would tell an
    // author their over-cap interval published.
    let mut shape = clean_plan();
    shape.frequency = Some(crate::domain::plan_shape::Frequency::CustomEveryN {
        n: 400,
        unit: crate::domain::plan_shape::CustomIntervalUnit::Days,
    });

    let report = run_publish_rules(&shape, &params_capped(100, 500));

    assert!(!report.is_publishable(), "a hard cap blocks");
    assert!(
        codes(&report).contains(&INVALID_CUSTOM_INTERVAL.to_owned()),
        "{:?}",
        codes(&report)
    );
    assert!(
        advisories(&report).is_empty(),
        "and produces no advisory: {:?}",
        advisories(&report)
    );
}

// ---------------------------------------------------------------------------
// inst-el-fields — the horizon belongs to its class (D-147)
// ---------------------------------------------------------------------------

/// One record filed under `class`, carrying a grandfathering horizon.
fn horizoned(price_id: u128, class: PriceEligibility) -> PriceRecord {
    let mut record = record(price_id, Some(ModelKind::Flat), Some("half_up"));
    record.scope_key = ScopeKey::new(
        plan(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("non-blank"),
        PhaseId::new(Uuid::from_u128(0xf1)),
        class,
        ChargeKind::Recurring,
        match class {
            // The cohort biconditional (D-147's sibling): a grandfathered class
            // pairs with a generation, and every other class with none.
            PriceEligibility::ExistingGrandfathered => Cohort::Generation(now()),
            _ => Cohort::None,
        },
    )
    .expect("the class pairs with its cohort");
    record.grandfather_until = Some(now());
    record
}

#[test]
fn a_horizon_on_a_non_grandfathered_row_is_refused_from_the_publish_path() {
    // The half D-147 left unbuilt. The repository already refuses this on both
    // authoring paths; what was missing is the report line, so a publish's 422
    // enumerates it beside every other violation instead of the first authoring
    // write happening to have caught it.
    let mut shape = clean_plan();
    shape.rows = vec![horizoned(0xb301, PriceEligibility::AllSubscriptions)];

    let report = run_publish_rules(&shape, &params(Some("half_up")));

    assert!(!report.is_publishable());
    assert!(
        codes(&report).contains(&GRANDFATHER_UNTIL_FORBIDDEN.to_owned()),
        "{:?}",
        codes(&report)
    );
    let violation = report
        .violations
        .iter()
        .find(|v| v.code == GRANDFATHER_UNTIL_FORBIDDEN)
        .expect("the finding");
    assert!(
        violation
            .subject
            .contains(&Uuid::from_u128(0xb301).to_string()),
        "the row is named: {}",
        violation.subject
    );
}

#[test]
fn a_horizon_on_a_grandfathered_row_is_exactly_what_the_field_is_for() {
    let mut shape = clean_plan();
    shape.rows = vec![horizoned(0xb302, PriceEligibility::ExistingGrandfathered)];

    let report = run_publish_rules(&shape, &params(Some("half_up")));

    assert!(
        !codes(&report).contains(&GRANDFATHER_UNTIL_FORBIDDEN.to_owned()),
        "{:?}",
        codes(&report)
    );
}

#[test]
fn the_horizon_finding_appears_in_the_same_report_as_another_rows_violation() {
    // The enumerate-all contract. Before this rule, a plan with a misfiled
    // horizon and a shape fault elsewhere reported the shape fault and left the
    // horizon to whichever authoring write reached the store first.
    let mut shape = clean_plan();
    shape.rows = vec![
        horizoned(0xb301, PriceEligibility::NewSubscriptionsOnly),
        // No model kind: a Slice-3 row fault on a different row.
        record(0xb303, None, Some("half_up")),
    ];

    let found = codes(&run_publish_rules(&shape, &params(Some("half_up"))));

    assert!(
        found.contains(&GRANDFATHER_UNTIL_FORBIDDEN.to_owned()),
        "{found:?}"
    );
    assert!(found.contains(&MODEL_KIND_MISSING.to_owned()), "{found:?}");
}

#[test]
fn the_row_type_gained_no_field_and_the_generation_moved_only_for_a_rostered_one() {
    // D-162's boundary, asserted rather than trusted. `grandfather_until` lives
    // on `PriceRecord`; reaching it by widening `PriceRow` would move a field
    // across the roster boundary and imply a generation bump for something that
    // is not evaluation policy at all.
    let row = crate::domain::price_row::PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    let (rostered, outside) = crate::domain::evaluation_policy::partition_row_fields(&row);
    assert!(
        !rostered.iter().any(|field| field.contains("grandfather")),
        "the horizon is not an evaluation-policy field: {rostered:?}"
    );
    assert!(
        !outside.iter().any(|field| field.contains("grandfather")),
        "nor a PriceRow field at all: {outside:?}"
    );
    assert_eq!(
        crate::domain::evaluation_policy::EVALUATION_POLICY_GENERATION,
        "ep-4",
        "the generation moved to `ep-2` for `usage_counter_on_plan_change` (D-113, Slice 6), a \
         plan-scoped field a snapshot freezes, to `ep-3` for `reservation_flavor` (D-53, \
         Slice 10), and to `ep-4` for `min_qty_usage` + `min_qty_usage_fallback` (D-68) -- a \
         usage floor decides what quantity is billable and its fallback decides what happens \
         beneath it, which is quantity derivation. Their partners went outside: \
         `min_qty_purchase` is an order-time permission and `discount_ref` names an instrument \
         another gear evaluates -- the argument for the second bump: the flavor decides whether the reserved \
         quantity leaves the on-demand tier counter (`inst-rv-tier-q`) or never enters it \
         (`inst-rv-level`), which is quantity derivation and therefore evaluation policy by the \
         roster's own test. Its partner `reserved_rate` is the row's **price** and is filed \
         outside, so it moves no generation. It is a **rate** and not a whole-minor amount -- \
         `pricing_price.reserved_rate_nano` is the whole argument -- which is why the reason is \
         stated as `unit_rate`'s, the class it shares. \
         Neither is `grandfather_until`, which is still on no row at \
         all. This assertion is transcribed rather than derived, so a bump has to be argued for \
         here as well as in the document"
    );
}

// ---------------------------------------------------------------------------
// D-179 — publish refuses a stored Slice-10 primitive whose rules are unbuilt
// ---------------------------------------------------------------------------

/// The world in which the property is observable.
///
/// Without this the three refusals below would pass identically against a rule
/// that refused every plan, which is the defect class this program keeps paying
/// for: a test that never establishes the negative case is not evidence about
/// the positive one.
#[test]
fn a_plan_carrying_neither_unjudged_primitive_publishes() {
    let report = run_publish_rules(&clean_plan(), &params(Some("half_up")));
    assert!(
        report.is_publishable(),
        "the fixture the D-179 cases vary must itself be clean: {:?}",
        codes(&report)
    );
    assert!(
        !codes(&report).contains(&PRIMITIVE_RULES_UNBUILT.to_owned()),
        "and it must not be carrying the very code under test"
    );
}

/// D-45's field, refused at the act that would freeze it (D-179, D-177 §3).
#[test]
fn a_stored_row_carrying_an_included_allowance_is_refused_at_publish() {
    use crate::domain::price_row::{IncludedAllowance, RolloverPolicy};

    let mut shape = clean_plan();
    shape.rows[0].row.included_allowance = Some(IncludedAllowance {
        quantity: 100,
        rollover_policy: RolloverPolicy::Carry,
    });

    let report = run_publish_rules(&shape, &params(Some("half_up")));
    assert!(
        !report.is_publishable(),
        "publish is the freezing act and this value has no rule that judged it"
    );
    assert!(
        codes(&report).contains(&PRIMITIVE_RULES_UNBUILT.to_owned()),
        "and the refusal must name its own reason, got {:?}",
        codes(&report)
    );
}

/// D-40's field, on its own — one rule, two independent reasons.
#[test]
fn a_stored_row_carrying_a_tier_qualification_window_is_refused_at_publish() {
    use crate::domain::price_row::TierQualificationWindow;

    let mut shape = clean_plan();
    shape.rows[0].row.tier_qualification_window = Some(TierQualificationWindow::TrailingPeriod);

    let report = run_publish_rules(&shape, &params(Some("half_up")));
    assert!(!report.is_publishable());
    assert!(
        codes(&report).contains(&PRIMITIVE_RULES_UNBUILT.to_owned()),
        "got {:?}",
        codes(&report)
    );
}

/// The refusal names the row, because that is the edit — and both fields on one
/// row are two findings, not one, since clearing either leaves the other.
#[test]
fn both_fields_on_one_row_are_reported_separately_and_name_the_row() {
    use crate::domain::price_row::{IncludedAllowance, RolloverPolicy, TierQualificationWindow};

    let mut shape = clean_plan();
    shape.rows[0].row.included_allowance = Some(IncludedAllowance {
        quantity: 5,
        rollover_policy: RolloverPolicy::Carry,
    });
    shape.rows[0].row.tier_qualification_window = Some(TierQualificationWindow::Current);
    let price_id = shape.rows[0].price_id.to_string();

    let report = run_publish_rules(&shape, &params(Some("half_up")));
    let mine: Vec<_> = report
        .violations
        .iter()
        .filter(|v| v.code == PRIMITIVE_RULES_UNBUILT)
        .collect();
    assert_eq!(
        mine.len(),
        2,
        "clearing one field leaves the other, so each is its own finding"
    );
    for violation in mine {
        assert_eq!(
            violation.subject, price_id,
            "the subject is the row the operator edits"
        );
    }
}

/// The list, at the seam all three doors read (D-45's landing narrowed it).
///
/// **The positive control this rule most needed.** Every case above hands the
/// rule a row it refuses, so all of them would pass identically against the
/// pre-D-45 arm that refused `includedAllowance` outright. The one assertion that
/// distinguishes the two is that a `{N, none}` declaration produces **nothing**:
/// it has a rule that judges it (`inst-ac-gate`) and a compile that honours it,
/// which is exactly what D-177 clause (3) required before the field could leave.
#[test]
fn the_unjudged_list_refuses_a_carried_allowance_and_admits_a_compiled_one() {
    use crate::domain::price_row::{IncludedAllowance, RolloverPolicy, TierQualificationWindow};

    let mut row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::Graduated));
    assert!(
        super::unjudged_primitives(&row).is_empty(),
        "a row declaring nothing carries nothing"
    );

    row.included_allowance = Some(IncludedAllowance {
        quantity: 100,
        rollover_policy: RolloverPolicy::None,
    });
    assert!(
        super::unjudged_primitives(&row).is_empty(),
        "rolloverPolicy = none is judged and compiled: it must not be refused as unbuilt"
    );

    row.included_allowance = Some(IncludedAllowance {
        quantity: 100,
        rollover_policy: RolloverPolicy::Carry,
    });
    let carried = super::unjudged_primitives(&row);
    assert_eq!(carried.len(), 1, "{carried:?}");
    assert!(
        carried[0].0.contains("carry"),
        "the entry names the half at fault rather than the field: {:?}",
        carried[0]
    );
    assert!(
        carried[0].1.contains("pricing_plan_grant"),
        "and says what is missing, which is a store rather than a rule: {:?}",
        carried[0]
    );

    row.tier_qualification_window = Some(TierQualificationWindow::Current);
    assert_eq!(
        super::unjudged_primitives(&row).len(),
        2,
        "clearing one leaves the other, so each is its own entry"
    );
}

/// The code is spelled as the design set spells it (`01-foundation.md` §3.3).
#[test]
fn the_unjudged_primitive_code_is_spelled_as_the_design_set_spells_it() {
    assert_eq!(PRIMITIVE_RULES_UNBUILT, "PRIMITIVE_RULES_UNBUILT");
}

/// D-179 clause (4): the gate is stable across re-entry.
///
/// The second run of a mechanism is the question this program has learned to
/// ask. This one consumes nothing and clears nothing, so a re-submitted publish
/// reads the same stored row and takes the same refusal — it is not a one-shot
/// that a retry walks past.
#[test]
fn the_refusal_is_the_same_on_the_second_run() {
    use crate::domain::price_row::TierQualificationWindow;

    let mut shape = clean_plan();
    shape.rows[0].row.tier_qualification_window = Some(TierQualificationWindow::Current);

    let first = run_publish_rules(&shape, &params(Some("half_up")));
    let second = run_publish_rules(&shape, &params(Some("half_up")));
    // Assert the refusal is PRESENT in both before asserting they agree: two
    // empty reports also agree, so equality alone would pass against no rule at
    // all — the vacuous shape this file's other cases exist to avoid.
    for (label, report) in [("first", &first), ("second", &second)] {
        assert!(
            codes(report).contains(&PRIMITIVE_RULES_UNBUILT.to_owned()),
            "the {label} run must refuse, got {:?}",
            codes(report)
        );
    }
    assert_eq!(
        codes(&first),
        codes(&second),
        "a re-entered publish must take the same refusal, not walk past a spent one"
    );
}

// ---------------------------------------------------------------------------
// inst-bc-taxbasis's reverse half — D-119's guard, homed here by D-212
// ---------------------------------------------------------------------------

/// One market of one bundle that references this plan as a component, and the
/// tax basis its **other** members already established there.
fn referencing(
    bundle: u128,
    currency: &str,
    region: &str,
    tax_inclusive: bool,
) -> ReferencingMarket {
    ReferencingMarket::new(
        Uuid::from_u128(bundle),
        CurrencyCode::new(currency).expect("three letters"),
        Region::new(region).expect("non-blank"),
        tax_inclusive,
    )
}

/// `clean_plan` whose one row declares `tax_inclusive`.
fn plan_on_basis(tax_inclusive: bool) -> PlanShape {
    let mut shape = clean_plan();
    shape.rows[0].tax_inclusive = tax_inclusive;
    shape
}

#[test]
fn a_component_publish_that_would_mix_a_referencing_bundles_market_is_refused() {
    // D-119's reverse half. The forward half lives in the bundle's own validator
    // and is a property of a handed-in composition; this one is about *another*
    // plan's publish, so it is the component's pipeline that has to answer it —
    // otherwise the bundle-side check is a point-in-time promise a later
    // component publish silently breaks.
    let shape = plan_on_basis(false);
    let params = params(Some("half_up"))
        .with_referencing_markets(vec![referencing(0xb0_1d, "EUR", "eu", true)]);

    let report = run_publish_rules(&shape, &params);

    assert!(
        codes(&report).contains(&BUNDLE_TAX_BASIS_MIXED.to_owned()),
        "the publish must refuse: {:?}",
        codes(&report)
    );
    let named = report
        .violations
        .iter()
        .find(|violation| violation.code == BUNDLE_TAX_BASIS_MIXED)
        .expect("the violation is there");
    assert!(
        named.detail.contains(&Uuid::from_u128(0xb0_1d).to_string()),
        "D-119 requires the referencing bundle enumerated: {}",
        named.detail
    );
}

#[test]
fn a_component_publish_that_agrees_with_the_bundles_market_publishes() {
    // The control. Without it the case above passes against a rule that refuses
    // whenever any referencing market exists at all.
    let shape = plan_on_basis(true);
    let params = params(Some("half_up"))
        .with_referencing_markets(vec![referencing(0xb0_1d, "EUR", "eu", true)]);

    let report = run_publish_rules(&shape, &params);

    assert!(
        !codes(&report).contains(&BUNDLE_TAX_BASIS_MIXED.to_owned()),
        "an agreeing basis is not a mix: {:?}",
        codes(&report)
    );
}

#[test]
fn a_market_this_plan_prices_nothing_in_cannot_mix_anything() {
    // **D-212's first derivation, asserted rather than assumed.** The guard
    // ranges over the markets of *this publish's own candidate rows*, not over
    // the referencing bundle's declared market set — which a component's publish
    // does not carry (D-216 leaves it on the bundle's publish request). The
    // narrower set is complete because a basis conflict cannot arise in a market
    // this plan contributes no row to. The bundle here disagrees on `US`, where
    // the plan prices nothing.
    let shape = plan_on_basis(false);
    let params = params(Some("half_up")).with_referencing_markets(vec![
        referencing(0xb0_1d, "USD", "us", true),
        referencing(0xb0_1d, "EUR", "eu", false),
    ]);

    let report = run_publish_rules(&shape, &params);

    assert!(
        !codes(&report).contains(&BUNDLE_TAX_BASIS_MIXED.to_owned()),
        "only the markets this plan sells in are the guard's business: {:?}",
        codes(&report)
    );
}

#[test]
fn two_referencing_bundles_are_each_named() {
    // A component may sit in several bundles, and an operator fixing one basis
    // has to know every market the change breaks — one report, both bundles,
    // because a refusal naming one of two makes the second publish attempt fail
    // for a reason the first refusal did not mention.
    let shape = plan_on_basis(false);
    let params = params(Some("half_up")).with_referencing_markets(vec![
        referencing(0xb0_1d, "EUR", "eu", true),
        referencing(0xb0_2d, "EUR", "eu", true),
    ]);

    let report = run_publish_rules(&shape, &params);

    let detail: String = report
        .violations
        .iter()
        .filter(|violation| violation.code == BUNDLE_TAX_BASIS_MIXED)
        .map(|violation| violation.detail.clone())
        .collect::<Vec<_>>()
        .join(" | ");
    for bundle in [0xb0_1d_u128, 0xb0_2d] {
        assert!(
            detail.contains(&Uuid::from_u128(bundle).to_string()),
            "bundle {bundle:x} must be named: {detail}"
        );
    }
}

// ---------------------------------------------------------------------------
// `inst-rc-union` (Slice 6) — the contract that the checks are run.
// ---------------------------------------------------------------------------

/// Every rule the rating-compatibility union names is registered on the publish
/// path.
///
/// This is what `inst-rc-union` actually promises Rating: not a second opinion
/// on any of these — that would be a second registration, free to disagree with
/// the rule that owns it — but that the checks **run** when a plan publishes. The
/// only thing that can break the promise is a rule quietly leaving a pipeline,
/// and that is exactly what this catches.
///
/// The names are read off the pipelines rather than the aggregate report,
/// because a rule that is registered and finds nothing is still registered, and
/// a report-based census could only see rules that happened to fire.
#[test]
fn every_rule_the_rating_compatibility_union_names_is_registered() {
    use crate::domain::contracts::RATING_COMPAT_UNION;

    let params = params(Some("half_up"));
    let mut registered: Vec<&'static str> = crate::domain::rules::price_row_rules().rule_names();
    registered.extend(super::foundation_plan_rules(&params).rule_names());
    registered.extend(
        crate::domain::plan_rules::plan_shape_rules(params.interval_bounds, params.descriptors)
            .rule_names(),
    );

    let missing: Vec<&&str> = RATING_COMPAT_UNION
        .iter()
        .filter(|id| !registered.contains(id))
        .collect();

    assert!(
        missing.is_empty(),
        "the rating-compatibility union names rules the publish path does not run: {missing:?}. \
         Either a rule left a pipeline, or the roster in `domain::contracts` names one that was \
         never there"
    );
}

/// **The Foundation set's roster, exactly and in order.**
///
/// The census above answers a narrower question — every rule
/// `RATING_COMPAT_UNION` names is registered *somewhere* on the publish path —
/// and three of the rules registered here are named by no roster at all, so
/// deleting their `.with_rule(…)` lines left this crate green: `inst-td-policy`,
/// `inst-td-basis-uniform` and `inst-cb-addon` were the ones measured, and with
/// them gone a plan mixing tax display basis across a market, or leaving a
/// required addon uncovered in a currency, published unjudged.
///
/// Set equality rather than a containment floor, for the reason
/// `plan_rules_tests::REGISTERED` gives: a `contains` census passes while the
/// population shrinks, and the *order* is a property too — `RoundingPolicyResolved`
/// runs before the two taxonomy rules so an unresolvable policy is reported as
/// itself rather than as an undeclared token.
///
/// A rule *leaving* this set is the fault this catches; a rule *joining* it lands
/// here as a one-line edit, which is the visit that makes the addition
/// deliberate.
const FOUNDATION_REGISTERED: &[&str] = &[
    "foundation.rounding_policy_resolved",
    "nfr-size-limits",
    // `GrandfatherHorizonOnItsClass` reports under the eligibility instruction
    // rather than a `foundation.*` name of its own.
    "inst-el-fields",
    "foundation.no_unjudged_primitive",
    "foundation.bundle_market_basis_unmixed",
    // Slice 4's taxonomy pair, in the base set because `region` is a scope-key
    // axis and `rounding_policy_ref` resolves through the tenant default.
    "inst-tx-region",
    "inst-tx-rounding",
    // D-356's GL-code vocabulary: the descriptor's one `glCode` against the
    // tenant's declared set, beside the other two vocabulary rules.
    "inst-ds-glcode",
    // Slice 4's tax-display pair: `TAX_BASIS_INCOMPLETE` and
    // `TAX_BASIS_MIXED_MARKET`.
    "inst-td-policy",
    "inst-td-basis-uniform",
    // `inst-cb-addon` case (i): `CURRENCY_NOT_COVERED`.
    "inst-cb-addon",
];

#[test]
fn the_foundation_set_registers_exactly_its_roster_in_order() {
    let params = params(Some("half_up"));

    assert_eq!(
        super::foundation_plan_rules(&params).rule_names(),
        FOUNDATION_REGISTERED
    );
}
