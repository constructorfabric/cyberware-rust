//! Tests for the Slice-2 rule-code contract.
//!
//! The constants are a contract with three parties that never see this crate's
//! source: the conformance corpus, an RFC 9457 consumer, and
//! `design/02-plan-definition.md` §5, which spells every one of them. A
//! constant whose value drifted from its name would still compile, would still
//! be referenced at every `report.violate` site, and would be wrong on the
//! wire — so the spelling is asserted byte for byte against §5 rather than
//! against the identifier.

use std::collections::BTreeSet;

use uuid::Uuid;

use super::{CustomIntervalBounds, DescriptorSetComplete, plan_shape_rules};
use crate::domain::instant::utc_ymd_hms;
use crate::domain::plan_shape::{
    AddonRule, BillingCycle, CustomIntervalUnit, Frequency, PlanShape,
};
use crate::domain::rules;
use crate::domain::scope_key::PlanId;
use crate::domain::validation::ValidationPipeline;

use super::{
    ADDON_CYCLE, ADDON_INCOMPATIBLE, ADDON_QTY_RANGE_INVALID, AVAILABLE_FROM_IN_PAST,
    BASE_MARKET_INCOMPLETE, CYCLE_METADATA_MISSING, DESCRIPTOR_INCOMPLETE,
    DISPLAY_TRIAL_DAYS_INVALID, HYBRID_INCOMPLETE, INVALID_CUSTOM_INTERVAL, METER_AMBIGUOUS,
    PERIOD_FLOOR_CAP_AMOUNT_INVALID, PERIOD_FLOOR_CAP_MARKET_UNSOLD, PHASE_CHAIN_NONLINEAR,
    PHASE_DURATION_INVALID, PHASE_GRAPH_INVALID, PHASE_IN_USE, PHASE_OVERRIDE_ORPHANED,
    PHASE_OVERRIDE_UNIT_MISMATCH, PHASE_ROW_ORPHANED, PHASE_UNCOVERED, PLAN_NAME_INVALID,
    PLANTIER_MISSING, PURCHASE_QTY_RANGE_INVALID, SETUP_ROW_INVALID, TERMINAL_PHASE_CHANGED,
    TERMINAL_PHASE_KIND_INVALID, USAGE_MARKET_INCOMPLETE,
};
use crate::domain::rules::{COMPOSITE_SELF_REFERENCE, COMPOSITE_TOO_FEW_CONSTITUENTS};

/// Every code this pipeline emits, paired with the spelling its own slice's §5
/// gives it — `02-plan-definition.md` for G4's, and `10-advanced-primitives.md`
/// for Slice 10's two (D-256), which this Slice-2 pipeline also registers because
/// a composite is plan-shape configuration. The right-hand side is transcribed from the document, not from the
/// constant, which is the only arrangement under which this test can fail.
///
/// **It said "every code this pipeline emits" and held 21 of `plan_rules`' 28
/// until 2026-08-18** (review B8-1). The seven outside it were each raised by a
/// registered rule, and three of them — `BASE_MARKET_INCOMPLETE`,
/// `ADDON_QTY_RANGE_INVALID`, `DISPLAY_TRIAL_DAYS_INVALID` — had no byte-for-byte
/// spelling pin anywhere in the crate or its integration suites, which is exactly
/// the exposure the module doc above names. `BASE_MARKET_INCOMPLETE` is the
/// sharpest: this file *discusses* it a few dozen lines below, as the code D-149
/// clause 1 minted, while never adding it here.
///
/// `every_code_this_pipeline_declares_is_in_the_list` is what keeps that from
/// happening again, and it is why the hard count `assert_eq!(DECLARED.len(), 23)`
/// is gone: a count is a good instrument pointed at whatever list somebody typed,
/// and it did nothing about the seven already outside.
const DECLARED: &[(&str, &str)] = &[
    // Slice 10's two, emitted by this pipeline since D-256.
    (
        COMPOSITE_TOO_FEW_CONSTITUENTS,
        "COMPOSITE_TOO_FEW_CONSTITUENTS",
    ),
    (COMPOSITE_SELF_REFERENCE, "COMPOSITE_SELF_REFERENCE"),
    (CYCLE_METADATA_MISSING, "CYCLE_METADATA_MISSING"),
    (BASE_MARKET_INCOMPLETE, "BASE_MARKET_INCOMPLETE"),
    (INVALID_CUSTOM_INTERVAL, "INVALID_CUSTOM_INTERVAL"),
    (HYBRID_INCOMPLETE, "HYBRID_INCOMPLETE"),
    (USAGE_MARKET_INCOMPLETE, "USAGE_MARKET_INCOMPLETE"),
    (SETUP_ROW_INVALID, "SETUP_ROW_INVALID"),
    (PURCHASE_QTY_RANGE_INVALID, "PURCHASE_QTY_RANGE_INVALID"),
    (PLAN_NAME_INVALID, "PLAN_NAME_INVALID"),
    (AVAILABLE_FROM_IN_PAST, "AVAILABLE_FROM_IN_PAST"),
    (PLANTIER_MISSING, "PLANTIER_MISSING"),
    (METER_AMBIGUOUS, "METER_AMBIGUOUS"),
    (ADDON_CYCLE, "ADDON_CYCLE"),
    (ADDON_INCOMPATIBLE, "ADDON_INCOMPATIBLE"),
    (ADDON_QTY_RANGE_INVALID, "ADDON_QTY_RANGE_INVALID"),
    (PHASE_GRAPH_INVALID, "PHASE_GRAPH_INVALID"),
    (PHASE_CHAIN_NONLINEAR, "PHASE_CHAIN_NONLINEAR"),
    (TERMINAL_PHASE_KIND_INVALID, "TERMINAL_PHASE_KIND_INVALID"),
    (PHASE_DURATION_INVALID, "PHASE_DURATION_INVALID"),
    (DISPLAY_TRIAL_DAYS_INVALID, "DISPLAY_TRIAL_DAYS_INVALID"),
    (PHASE_ROW_ORPHANED, "PHASE_ROW_ORPHANED"),
    (PHASE_UNCOVERED, "PHASE_UNCOVERED"),
    (PHASE_OVERRIDE_ORPHANED, "PHASE_OVERRIDE_ORPHANED"),
    (PHASE_OVERRIDE_UNIT_MISMATCH, "PHASE_OVERRIDE_UNIT_MISMATCH"),
    (TERMINAL_PHASE_CHANGED, "TERMINAL_PHASE_CHANGED"),
    (PHASE_IN_USE, "PHASE_IN_USE"),
    (DESCRIPTOR_INCOMPLETE, "DESCRIPTOR_INCOMPLETE"),
    (
        PERIOD_FLOOR_CAP_MARKET_UNSOLD,
        "PERIOD_FLOOR_CAP_MARKET_UNSOLD",
    ),
    (
        PERIOD_FLOOR_CAP_AMOUNT_INVALID,
        "PERIOD_FLOOR_CAP_AMOUNT_INVALID",
    ),
];

#[test]
fn every_declared_code_is_spelled_as_the_design_set_spells_it() {
    for (constant, in_the_document) in DECLARED {
        assert_eq!(constant, in_the_document);
    }
}

/// Every code `plan_rules` declares is in [`DECLARED`].
///
/// The completeness half, and it is **derived from the source** rather than
/// counted: `plan_rules.rs`'s own `pub const … : &str` declarations are the
/// operand, so a code minted there and not added above reddens here on the commit
/// that mints it. The list this replaced asserted a length of 23 against a module
/// that declared 28, and the shortfall was invisible precisely because a length is
/// a fact about the list rather than about the thing the list claims to cover.
///
/// Slice 10's two live in `rules.rs` and are extras rather than omissions, so they
/// are subtracted rather than sought.
#[test]
fn every_code_this_pipeline_declares_is_in_the_list() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/domain/plan_rules.rs"),
    )
    .expect("the module this list is the contract for");

    let declared_in_source: BTreeSet<&str> = source
        .lines()
        .filter_map(|line| line.strip_prefix("pub const "))
        .filter_map(|rest| rest.split_once(": &str = \""))
        .filter_map(|(_, value)| value.split_once("\";"))
        .map(|(value, _)| value)
        .collect();

    assert!(
        declared_in_source.len() >= 21,
        "the scan found {} codes in plan_rules.rs, fewer than the list it is checking already \
         held — a scan that silently matched nothing would pass this test vacuously",
        declared_in_source.len()
    );

    let listed: BTreeSet<&str> = DECLARED
        .iter()
        .map(|(code, _)| *code)
        .filter(|code| *code != COMPOSITE_TOO_FEW_CONSTITUENTS && *code != COMPOSITE_SELF_REFERENCE)
        .collect();

    assert_eq!(
        listed, declared_in_source,
        "a code plan_rules declares is missing from DECLARED, or DECLARED names one it does \
         not declare; the spelling on the right of each pair is transcribed from section 5, so adding \
         an entry means reading the document rather than copying the constant"
    );
}

#[test]
fn no_two_codes_share_a_spelling() {
    // A discriminator that names two rules cannot discriminate, and a
    // copy-paste in the list above is exactly how that happens.
    let unique: BTreeSet<&str> = DECLARED.iter().map(|(code, _)| *code).collect();

    assert_eq!(unique.len(), DECLARED.len());
}

// ---------------------------------------------------------------------------
// The registered pipeline.
// ---------------------------------------------------------------------------

/// The Slice-2 pipeline, as a caller builds it.
fn pipeline() -> ValidationPipeline<PlanShape> {
    plan_shape_rules(
        // Real caps rather than a placeholder: `CustomIntervalBounds` has no
        // `Default` because zero caps reject every custom frequency ever
        // authored, and a test that passed zeros would exercise that state
        // rather than the configured one. These are `LimitsConfig`'s defaults,
        // which are PRD 14's ratified values.
        CustomIntervalBounds::new(366, 24),
        DescriptorSetComplete::default(),
    )
}

/// Every rule the Slice-2 pipeline registers, in the order it registers them.
///
/// Transcribed from the four algorithms of `02-plan-definition.md` 3 rather than
/// read off the pipeline, which is the only arrangement under which this test
/// can fail: a rule dropped from the registration is invisible otherwise. Its
/// module still compiles, its own unit tests still pass, and the plan it exists
/// to reject publishes.
const REGISTERED: &[&str] = &[
    // cpt-cf-bss-pricing-algo-cycle-shape
    //
    // `inst-cs-declared` is FIRST, and the order is the rule's reason for
    // existing (D-149 clause 2): every rule after it is cycle-conditioned, so a
    // NULL cycle passed the whole step vacuously.
    "inst-cs-declared",
    "inst-cs-customfreq",
    "inst-cs-hybrid",
    "inst-cs-recurring",
    "inst-cs-usage",
    "inst-cs-setup",
    "inst-cs-onetime",
    "inst-cs-availability",
    // cpt-cf-bss-pricing-algo-composition
    // The name is judged before the tier: an author who left both wrong reads
    // the label fault first, and it is the cheaper of the two to fix.
    "inst-cmp-planname",
    "inst-cmp-plantier",
    "inst-cmp-injective",
    "inst-cmp-addons",
    "inst-cmp-addons/acyclic",
    "inst-cmp-addons/conflict",
    "inst-cmp-addons/qty",
    // cpt-cf-bss-pricing-algo-composite-meter (Slice 10, D-256). Registered in
    // this Slice-2 pipeline because a composite is plan-shape configuration and
    // the self-reference walk needs the whole revision's set.
    "inst-cm-constituents",
    "inst-cm-formula",
    // cpt-cf-bss-pricing-algo-phases
    "inst-ph-graph",
    "inst-ph-graph/linear",
    "inst-ph-graph/terminal-kind",
    "inst-ph-duration",
    "inst-ph-trial",
    // Immediately before `inst-ph-coverage`, and the adjacency is the contract
    // (D-337): the two are exact inverses over one relation — a row naming a
    // phase the revision does not attach, a phase no row covers — so a report
    // carrying both names them together.
    "inst-ph-row-attached",
    "inst-ph-coverage",
    "inst-ph-usage-invariant",
    "inst-ph-override-units",
    "inst-ph-terminal-stable",
    // cpt-cf-bss-pricing-algo-period-floor-cap (D-319). Between the phase
    // schedule and the descriptors: a period bound is a fact about a whole
    // period's total, so it reads after the rules that establish what the
    // periods are.
    "inst-pfc-amount",
    "inst-pfc-market",
    // cpt-cf-bss-pricing-algo-descriptors
    "inst-ds-required",
];

#[test]
fn the_pipeline_registers_every_slice_2_rule_in_report_order() {
    assert_eq!(pipeline().rule_names(), REGISTERED);
}

/// The rules Slice 2 **cross-references and never registers** (Global Constraint
/// 5). A second registration of any of them is two owners of one rule, free to
/// disagree — and the disagreement is invisible, because both readings look
/// right at their own call site.
#[test]
fn no_rule_of_another_slice_is_registered_here() {
    let names: BTreeSet<&str> = pipeline().rule_names().into_iter().collect();

    // `billingGranularity` and `tierAggregationWindow` are Slice 3's, already
    // registered as `EVAL_POLICY_MISSING` in `rules::price_row_rules`;
    // `taxCategory` is each row's Slice-4 `tax_category_ref` (D-110).
    //
    // **`inst-cs-recurring` used to be on this list and is not any more.** Its
    // `billingTiming` clause is still Slice 6's and still cross-referenced, but
    // D-149 clause 1 gave the instruction a rule of its own -
    // `BASE_MARKET_INCOMPLETE` - which is Slice 2's outright. Excluding the whole
    // instruction because one of its three clauses is foreign is what left it
    // registering nothing at all.
    for foreign in ["inst-tb-window", "inst-pk-window", "inst-td-persist"] {
        assert!(
            !names.contains(foreign),
            "{foreign} belongs to another slice and must not be registered here"
        );
    }
    // And the Slice-3 pipeline is still the only home of the row-local rules,
    // which run over a different subject entirely.
    assert!(!rules::price_row_rules().rule_names().is_empty());
}

/// One deliberately awful plan, one report, **every** finding in it.
///
/// This is the fail-closed contract's whole point (1-foundation.md 4.2): the
/// pipeline never short-circuits, so an author who submits a plan that is wrong
/// in nine ways is told about nine of them and remediates in one pass. A
/// pipeline that stopped at the first violation would still block the publish
/// and would still look correct from the outside.
#[test]
fn one_awful_plan_produces_every_finding_in_one_report() {
    let plan_id = PlanId::new(Uuid::from_u128(0x9_1a4));
    let now = utc_ymd_hms(2026, 8, 2, 12, 0, 0);
    let analytics = Uuid::from_u128(0xadd01);
    let support = Uuid::from_u128(0xadd02);

    let mut plan = PlanShape::new(plan_id, 2, now);
    // A hybrid with no rows at all: both mandatory parts missing.
    plan.billing_cycle = Some(BillingCycle::Hybrid);
    // A custom frequency that recurs every zero days.
    plan.frequency = Some(Frequency::CustomEveryN {
        n: 0,
        unit: CustomIntervalUnit::Days,
    });
    // A purchase window that admits no quantity.
    plan.purchase_min_qty = Some(5);
    plan.purchase_max_qty = Some(2);
    // Newly set, and in the past.
    plan.available_from = Some(now - time::Duration::hours(1));
    // No tier, no phases, no descriptor set.
    plan.addon_rules = vec![
        AddonRule {
            addon_sku_id: analytics,
            required: true,
            min_qty: None,
            max_qty: Some(1),
            step_qty: None,
            price_override_ref: None,
            depends_on: vec![support],
            conflicts_with: vec![support],
        },
        AddonRule {
            addon_sku_id: support,
            required: true,
            min_qty: None,
            max_qty: Some(1),
            step_qty: None,
            price_override_ref: None,
            depends_on: vec![analytics],
            conflicts_with: vec![analytics],
        },
    ];

    let report = pipeline().run(&plan);

    assert!(!report.is_publishable());
    let codes: Vec<&str> = report
        .violations
        .iter()
        .map(|violation| violation.code.as_str())
        .collect();
    // In registration order, and every one of them: the cycle faults, then the
    // composition faults, then the phase graph, then each missing descriptor.
    assert_eq!(
        codes,
        [
            INVALID_CUSTOM_INTERVAL,
            HYBRID_INCOMPLETE,
            HYBRID_INCOMPLETE,
            PURCHASE_QTY_RANGE_INVALID,
            AVAILABLE_FROM_IN_PAST,
            PLANTIER_MISSING,
            ADDON_CYCLE,
            ADDON_INCOMPATIBLE,
            PHASE_GRAPH_INVALID,
            DESCRIPTOR_INCOMPLETE,
            DESCRIPTOR_INCOMPLETE,
            DESCRIPTOR_INCOMPLETE,
        ]
    );
}
