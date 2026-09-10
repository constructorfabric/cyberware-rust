//! Slice 6's consumer-contract rules — `design/06-consumer-contracts.md` §3, §5.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;

use uuid::Uuid;

use super::consumer_contract_rules;
use super::{
    AnchorDay, BILLING_TIMING_MISSING, BillingAnchorPolicy, BillingTimingPresent,
    PRORATION_CONTRACT_MIXED_MARKET, PRORATION_INPUTS_CONTRADICTORY, PRORATION_INPUTS_MISSING,
    ProrationBasis, ProrationContract, ProrationContractMarketUniform, ProrationCreditHasBasis,
    ProrationInputsPresent,
};
use super::{
    CHANGE_TARGET_UNPUBLISHED, COMPARABILITY_RANK_REQUIRED, COMPARABILITY_RANK_REVOKED,
    ChangeGraphAuthorable, ChangeTargetIndex, PlanChangeContract, UsageCounterOnPlanChange,
};
use super::{EntitlementGrants, GRANT_SET_PHASE_UNKNOWN, GrantSet, GrantSetPhasesKnown};
use crate::domain::concurrency::RowVersion;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount};
use crate::domain::plan_shape::{PhaseGraph, PhaseKind, PlanPhase, PlanShape};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::PriceRow;
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::validation::{ValidationReport, ValidationRule};
use time::OffsetDateTime;

fn now() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 7, 12, 0, 0)
}

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x0600))
}

/// One candidate row: its kind, its market, its eligibility and its timing.
fn row(
    price_id: u128,
    charge_kind: ChargeKind,
    currency: &str,
    region: &str,
    eligibility: PriceEligibility,
    billing_timing: Option<&str>,
    proration_contract: Option<ProrationContract>,
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
        charge_kind,
        cohort,
    )
    .expect("the eligibility and cohort pair");

    let mut shape = PriceRow::new(charge_kind, None);
    shape.amount_minor = Some(MinorAmount::new(1000).expect("non-negative"));

    PriceRecord {
        price_id: Uuid::from_u128(price_id),
        scope_key,
        row: shape,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: billing_timing.map(ToOwned::to_owned),
        proration_contract,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Draft,
        created_by: Uuid::from_u128(0x06_ac),
        created_at_utc: now(),
        row_version: RowVersion::new(0),
    }
}

fn shape_of(rows: Vec<PriceRecord>) -> PlanShape {
    let mut shape = PlanShape::new(plan(), 1, now());
    shape.rows = rows;
    shape
}

fn run(rule: &impl ValidationRule<PlanShape>, shape: &PlanShape) -> ValidationReport {
    let mut report = ValidationReport::default();
    rule.evaluate(shape, &mut report);
    report
}

fn codes(report: &ValidationReport) -> Vec<String> {
    report.violations.iter().map(|v| v.code.clone()).collect()
}

// ---------------------------------------------------------------------------
// `inst-bt-required` (§3 Billing Timing step 1, §5 `BILLING_TIMING_MISSING`).
// ---------------------------------------------------------------------------

#[test]
fn the_code_is_the_designs_verbatim() {
    assert_eq!(BILLING_TIMING_MISSING, "BILLING_TIMING_MISSING");
}

#[test]
fn a_recurring_row_without_billing_timing_fails_publish_naming_the_row() {
    let shape = shape_of(vec![row(
        0xb1,
        ChargeKind::Recurring,
        "EUR",
        "eu",
        PriceEligibility::AllSubscriptions,
        None,
        None,
    )]);

    let report = run(&BillingTimingPresent, &shape);

    assert_eq!(codes(&report), [BILLING_TIMING_MISSING]);
    let violation = &report.violations[0];
    assert_eq!(
        violation.subject,
        Uuid::from_u128(0xb1).to_string(),
        "the design's step 1 requires the row to be named"
    );
}

#[test]
fn a_recurring_row_stating_its_timing_passes() {
    for timing in ["advance", "arrears"] {
        let shape = shape_of(vec![row(
            0xb2,
            ChargeKind::Recurring,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            Some(timing),
            None,
        )]);

        assert!(
            codes(&run(&BillingTimingPresent, &shape)).is_empty(),
            "{timing} is one of the two the contract admits"
        );
    }
}

/// `inst-bt-usage`: usage and one-time rows never author the field — it is a
/// projected constant — so demanding it of them would reject a row whose value
/// is not the author's to give.
#[test]
fn a_non_recurring_row_without_billing_timing_is_not_this_rules_business() {
    for kind in [
        ChargeKind::Usage,
        ChargeKind::OneTime,
        ChargeKind::OneTimeSetup,
    ] {
        let shape = shape_of(vec![row(
            0xb3,
            kind,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            None,
            None,
        )]);

        assert!(
            codes(&run(&BillingTimingPresent, &shape)).is_empty(),
            "{kind} authors no billingTiming"
        );
    }
}

/// D-132 excludes a grandfathered generation from the *uniformity* row set —
/// an argument about comparing a frozen generation against current rows. It
/// says nothing about presence, and a grandfathered row published without the
/// field would leave Billing deriving a live subscriber's deferral by
/// heuristic.
#[test]
fn a_grandfathered_recurring_row_is_held_to_the_same_presence_rule() {
    let shape = shape_of(vec![row(
        0xb4,
        ChargeKind::Recurring,
        "EUR",
        "eu",
        PriceEligibility::ExistingGrandfathered,
        None,
        None,
    )]);

    assert_eq!(
        codes(&run(&BillingTimingPresent, &shape)),
        [BILLING_TIMING_MISSING]
    );
}

/// The report is the product: two bad rows are two findings, not the first one.
#[test]
fn every_offending_row_is_reported_not_merely_the_first() {
    let shape = shape_of(vec![
        row(
            0xb5,
            ChargeKind::Recurring,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            None,
            None,
        ),
        row(
            0xb6,
            ChargeKind::Recurring,
            "USD",
            "us",
            PriceEligibility::AllSubscriptions,
            None,
            None,
        ),
    ]);

    assert_eq!(codes(&run(&BillingTimingPresent, &shape)).len(), 2);
}

// ---------------------------------------------------------------------------
// `inst-pi-required` / `inst-pi-credit-none` / `inst-pi-uniform`
// (§3 Proration Input Contract, §5, D-123 scoped by D-132).
// ---------------------------------------------------------------------------

/// One contract, named by its three members.
fn contract(
    policy: BillingAnchorPolicy,
    basis: ProrationBasis,
    credit_on_downgrade: bool,
) -> ProrationContract {
    ProrationContract {
        billing_anchor_policy: policy,
        proration_basis: basis,
        credit_on_downgrade,
    }
}

/// The contract every uniformity case moves exactly one member away from.
fn baseline_contract() -> ProrationContract {
    contract(
        BillingAnchorPolicy::CalendarMonth,
        ProrationBasis::CalendarDaysActual,
        false,
    )
}

/// A recurring row on `currency`/`region`, in `eligibility`, carrying `c`.
fn priced(
    price_id: u128,
    currency: &str,
    region: &str,
    eligibility: PriceEligibility,
    c: Option<ProrationContract>,
) -> PriceRecord {
    row(
        price_id,
        ChargeKind::Recurring,
        currency,
        region,
        eligibility,
        Some("advance"),
        c,
    )
}

#[test]
fn the_three_codes_are_the_designs_verbatim() {
    assert_eq!(PRORATION_INPUTS_MISSING, "PRORATION_INPUTS_MISSING");
    assert_eq!(
        PRORATION_INPUTS_CONTRADICTORY,
        "PRORATION_INPUTS_CONTRADICTORY"
    );
    assert_eq!(
        PRORATION_CONTRACT_MIXED_MARKET,
        "PRORATION_CONTRACT_MIXED_MARKET"
    );
}

#[test]
fn a_recurring_row_without_the_proration_inputs_fails_publish_naming_the_row() {
    let shape = shape_of(vec![priced(
        0xc1,
        "EUR",
        "eu",
        PriceEligibility::AllSubscriptions,
        None,
    )]);

    let report = run(&ProrationInputsPresent, &shape);

    assert_eq!(codes(&report), [PRORATION_INPUTS_MISSING]);
    assert_eq!(
        report.violations[0].subject,
        Uuid::from_u128(0xc1).to_string()
    );
}

#[test]
fn a_recurring_row_stating_the_contract_passes() {
    let shape = shape_of(vec![priced(
        0xc2,
        "EUR",
        "eu",
        PriceEligibility::AllSubscriptions,
        Some(baseline_contract()),
    )]);

    assert!(codes(&run(&ProrationInputsPresent, &shape)).is_empty());
}

/// The contract attaches to recurring rows. A usage or one-time row authors no
/// proration inputs, and demanding them would reject a row whose values are not
/// the author's to give — the same boundary `inst-bt-required` draws.
#[test]
fn a_non_recurring_row_owes_no_proration_contract() {
    for kind in [
        ChargeKind::Usage,
        ChargeKind::OneTime,
        ChargeKind::OneTimeSetup,
    ] {
        let shape = shape_of(vec![row(
            0xc3,
            kind,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            None,
            None,
        )]);

        assert!(
            codes(&run(&ProrationInputsPresent, &shape)).is_empty(),
            "{kind} authors no proration contract"
        );
    }
}

/// A grandfathered row is held to **presence** — D-132 excludes it from the
/// uniformity row set, which is a different rule.
#[test]
fn a_grandfathered_recurring_row_still_owes_its_own_contract() {
    let shape = shape_of(vec![priced(
        0xc4,
        "EUR",
        "eu",
        PriceEligibility::ExistingGrandfathered,
        None,
    )]);

    assert_eq!(
        codes(&run(&ProrationInputsPresent, &shape)),
        [PRORATION_INPUTS_MISSING]
    );
}

#[test]
fn crediting_a_downgrade_with_no_basis_to_size_it_fails_publish() {
    let shape = shape_of(vec![priced(
        0xc5,
        "EUR",
        "eu",
        PriceEligibility::AllSubscriptions,
        Some(contract(
            BillingAnchorPolicy::CalendarMonth,
            ProrationBasis::None,
            true,
        )),
    )]);

    let report = run(&ProrationCreditHasBasis, &shape);

    assert_eq!(codes(&report), [PRORATION_INPUTS_CONTRADICTORY]);
    assert_eq!(
        report.violations[0].subject,
        Uuid::from_u128(0xc5).to_string()
    );
}

/// Both halves are individually legal: a plan that never prorates is a real
/// plan, and a credited downgrade is a real policy. Only the pair contradicts.
#[test]
fn neither_half_of_the_contradiction_is_a_violation_on_its_own() {
    for c in [
        contract(
            BillingAnchorPolicy::CalendarMonth,
            ProrationBasis::None,
            false,
        ),
        contract(
            BillingAnchorPolicy::CalendarMonth,
            ProrationBasis::BySecond,
            true,
        ),
    ] {
        let shape = shape_of(vec![priced(
            0xc6,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            Some(c),
        )]);
        assert!(
            codes(&run(&ProrationCreditHasBasis, &shape)).is_empty(),
            "{c:?} is publishable on its own"
        );
    }
}

/// D-123's own scenario: an intro-pricing plan anchoring `subscription_start`
/// on the interim row and `fixed_day(1)` on the terminal row, both on one market.
#[test]
fn two_anchors_on_one_market_fail_publish_naming_the_divergent_rows() {
    let shape = shape_of(vec![
        priced(
            0xd1,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            Some(contract(
                BillingAnchorPolicy::SubscriptionStart,
                ProrationBasis::CalendarDaysActual,
                false,
            )),
        ),
        priced(
            0xd2,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            Some(contract(
                BillingAnchorPolicy::FixedDay(AnchorDay::new(1).expect("a day of the month")),
                ProrationBasis::CalendarDaysActual,
                false,
            )),
        ),
    ]);

    let report = run(&ProrationContractMarketUniform, &shape);

    assert_eq!(codes(&report), [PRORATION_CONTRACT_MIXED_MARKET]);
    let detail = &report.violations[0].detail;
    for id in [0xd1_u128, 0xd2] {
        assert!(
            detail.contains(&Uuid::from_u128(id).to_string()),
            "the design requires the divergent rows to be named: {detail}"
        );
    }
    assert!(
        detail.contains("billingAnchorPolicy"),
        "and the divergent field: {detail}"
    );
}

/// Two `fixed_day` anchors on different days are two anchors, not one. A
/// comparison on the token alone would call them equal and publish a market
/// with two cycle clocks.
#[test]
fn two_fixed_days_on_different_days_are_a_divergence() {
    let shape = shape_of(vec![
        priced(
            0xd3,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            Some(contract(
                BillingAnchorPolicy::FixedDay(AnchorDay::new(1).expect("a day")),
                ProrationBasis::CalendarDaysActual,
                false,
            )),
        ),
        priced(
            0xd4,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            Some(contract(
                BillingAnchorPolicy::FixedDay(AnchorDay::new(15).expect("a day")),
                ProrationBasis::CalendarDaysActual,
                false,
            )),
        ),
    ]);

    assert_eq!(
        codes(&run(&ProrationContractMarketUniform, &shape)),
        [PRORATION_CONTRACT_MIXED_MARKET]
    );
}

/// Per market, not per plan: the D-110 shape. EU on the 1st and US on signup
/// day is exactly what this rule leaves legal.
#[test]
fn the_same_split_across_two_markets_publishes() {
    let shape = shape_of(vec![
        priced(
            0xd5,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            Some(contract(
                BillingAnchorPolicy::FixedDay(AnchorDay::new(1).expect("a day")),
                ProrationBasis::CalendarDaysActual,
                false,
            )),
        ),
        priced(
            0xd6,
            "USD",
            "us",
            PriceEligibility::AllSubscriptions,
            Some(contract(
                BillingAnchorPolicy::SubscriptionStart,
                ProrationBasis::CalendarDaysActual,
                false,
            )),
        ),
    ]);

    assert!(codes(&run(&ProrationContractMarketUniform, &shape)).is_empty());
}

/// D-132: an immutable generation is not in the uniformity row set. Without
/// this, one cutover would permanently freeze the market's cycle clock and
/// every later publish would fail on a row nobody can fix.
#[test]
fn a_grandfathered_generation_diverging_from_the_current_rows_publishes() {
    let shape = shape_of(vec![
        priced(
            0xd7,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            Some(baseline_contract()),
        ),
        priced(
            0xd8,
            "EUR",
            "eu",
            PriceEligibility::ExistingGrandfathered,
            Some(contract(
                BillingAnchorPolicy::SubscriptionStart,
                ProrationBasis::BySecond,
                true,
            )),
        ),
    ]);

    assert!(codes(&run(&ProrationContractMarketUniform, &shape)).is_empty());
}

/// `new_subscriptions_only` **is** in the row set — D-123 names the two
/// eligibility classes it covers, and this is the second of them.
#[test]
fn a_new_subscriptions_only_row_is_inside_the_uniformity_set() {
    let shape = shape_of(vec![
        priced(
            0xd9,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            Some(baseline_contract()),
        ),
        priced(
            0xda,
            "EUR",
            "eu",
            PriceEligibility::NewSubscriptionsOnly,
            Some(contract(
                BillingAnchorPolicy::SubscriptionStart,
                ProrationBasis::CalendarDaysActual,
                false,
            )),
        ),
    ]);

    assert_eq!(
        codes(&run(&ProrationContractMarketUniform, &shape)),
        [PRORATION_CONTRACT_MIXED_MARKET]
    );
}

/// All three members are compared, and the finding names which one diverged.
#[test]
fn the_basis_and_the_credit_flag_are_compared_too_and_the_finding_names_the_field() {
    for (moved, field) in [
        (
            contract(
                BillingAnchorPolicy::CalendarMonth,
                ProrationBasis::BySecond,
                false,
            ),
            "prorationBasis",
        ),
        (
            contract(
                BillingAnchorPolicy::CalendarMonth,
                ProrationBasis::CalendarDaysActual,
                true,
            ),
            "creditOnDowngrade",
        ),
    ] {
        let shape = shape_of(vec![
            priced(
                0xe1,
                "EUR",
                "eu",
                PriceEligibility::AllSubscriptions,
                Some(baseline_contract()),
            ),
            priced(
                0xe2,
                "EUR",
                "eu",
                PriceEligibility::AllSubscriptions,
                Some(moved),
            ),
        ]);

        let report = run(&ProrationContractMarketUniform, &shape);
        assert_eq!(codes(&report), [PRORATION_CONTRACT_MIXED_MARKET]);
        assert!(
            report.violations[0].detail.contains(field),
            "{field} diverged: {}",
            report.violations[0].detail
        );
    }
}

/// A row with no contract at all is `inst-pi-required`'s finding, not this
/// one's: reporting the same row twice under two codes would make an author
/// remediate one omission in two places.
#[test]
fn an_absent_contract_is_not_a_divergence() {
    let shape = shape_of(vec![
        priced(
            0xe3,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            Some(baseline_contract()),
        ),
        priced(0xe4, "EUR", "eu", PriceEligibility::AllSubscriptions, None),
    ]);

    assert!(codes(&run(&ProrationContractMarketUniform, &shape)).is_empty());
}

// ---------------------------------------------------------------------------
// `inst-pc-targets` / `inst-pc-mutual` / `inst-pc-rank` / `inst-pc-failsafe`
// (§3 Plan-Change Contract, §5, K4, D-23, D-24, D-54).
// ---------------------------------------------------------------------------

fn other(n: u128) -> Uuid {
    Uuid::from_u128(0xf000 + n)
}

/// A plan shape carrying `contract` and no rows.
fn shape_with(contract: PlanChangeContract) -> PlanShape {
    let mut shape = PlanShape::new(plan(), 1, now());
    shape.change_contract = contract;
    shape
}

/// An index in which each `(plan, rank)` is published, and `inbound` point here.
fn index(published: &[(Uuid, Option<i32>)], inbound: &[Uuid]) -> ChangeTargetIndex {
    ChangeTargetIndex::new(
        published.iter().copied().collect(),
        inbound.iter().copied().collect(),
    )
}

fn graph(index: ChangeTargetIndex) -> ChangeGraphAuthorable {
    ChangeGraphAuthorable { index }
}

#[test]
fn the_change_graph_codes_are_the_designs_verbatim() {
    assert_eq!(CHANGE_TARGET_UNPUBLISHED, "CHANGE_TARGET_UNPUBLISHED");
    assert_eq!(COMPARABILITY_RANK_REQUIRED, "COMPARABILITY_RANK_REQUIRED");
    assert_eq!(COMPARABILITY_RANK_REVOKED, "COMPARABILITY_RANK_REVOKED");
}

/// `inst-pc-failsafe`: absence is a value with a reading, and it is the one
/// state this rule has nothing to say about.
#[test]
fn a_plan_offering_no_self_service_change_is_publishable_with_no_rank() {
    let shape = shape_with(PlanChangeContract::default());

    assert!(codes(&run(&graph(index(&[], &[])), &shape)).is_empty());
}

/// An author who clears their last edge is leaving self-service change, and K4
/// must not refuse the publish that does it.
#[test]
fn an_empty_edge_list_is_leaving_self_service_change_not_entering_it() {
    let shape = shape_with(PlanChangeContract {
        allowed_change_targets: Some(Vec::new()),
        comparability_rank: None,
        usage_counter_on_plan_change: UsageCounterOnPlanChange::Reset,
    });

    assert!(codes(&run(&graph(index(&[], &[])), &shape)).is_empty());
}

#[test]
fn a_dangling_target_fails_publish_naming_the_target() {
    let shape = shape_with(PlanChangeContract {
        allowed_change_targets: Some(vec![other(1)]),
        comparability_rank: Some(10),
        usage_counter_on_plan_change: UsageCounterOnPlanChange::Reset,
    });

    let report = run(&graph(index(&[], &[])), &shape);

    assert_eq!(codes(&report), [CHANGE_TARGET_UNPUBLISHED]);
    assert!(
        report.violations[0].detail.contains(&other(1).to_string()),
        "{}",
        report.violations[0].detail
    );
}

/// K4, the source half: a plan that names edges owes a rank.
#[test]
fn a_source_plan_with_edges_and_no_rank_fails_publish() {
    let shape = shape_with(PlanChangeContract {
        allowed_change_targets: Some(vec![other(1)]),
        comparability_rank: None,
        usage_counter_on_plan_change: UsageCounterOnPlanChange::Reset,
    });

    assert_eq!(
        codes(&run(&graph(index(&[(other(1), Some(5))], &[])), &shape)),
        [COMPARABILITY_RANK_REQUIRED]
    );
}

/// `inst-pc-mutual`, the target half: without the target's rank the runtime
/// classification A→B is uncomputable, so publish refuses the edge.
#[test]
fn a_published_target_without_a_rank_fails_publish() {
    let shape = shape_with(PlanChangeContract {
        allowed_change_targets: Some(vec![other(1)]),
        comparability_rank: Some(10),
        usage_counter_on_plan_change: UsageCounterOnPlanChange::Reset,
    });

    let report = run(&graph(index(&[(other(1), None)], &[])), &shape);

    assert_eq!(codes(&report), [COMPARABILITY_RANK_REQUIRED]);
    assert!(
        report.violations[0].detail.contains(&other(1).to_string()),
        "the target is named: {}",
        report.violations[0].detail
    );
}

#[test]
fn a_whole_edge_publishes_when_both_ends_carry_a_rank() {
    let shape = shape_with(PlanChangeContract {
        allowed_change_targets: Some(vec![other(1), other(2)]),
        comparability_rank: Some(10),
        usage_counter_on_plan_change: UsageCounterOnPlanChange::Carry,
    });

    let idx = index(&[(other(1), Some(5)), (other(2), Some(20))], &[]);
    assert!(codes(&run(&graph(idx), &shape)).is_empty());
}

/// D-54's reverse guard. Without it a rank-less re-publish leaves
/// already-published inbound edges unclassifiable at read time — the same
/// read-time drift D-23 cut rule-based targets to avoid.
#[test]
fn dropping_the_rank_while_inbound_edges_reference_the_plan_is_refused() {
    let shape = shape_with(PlanChangeContract::default());

    let report = run(&graph(index(&[], &[other(7), other(8)])), &shape);

    assert_eq!(codes(&report), [COMPARABILITY_RANK_REVOKED]);
    let detail = &report.violations[0].detail;
    for id in [other(7), other(8)] {
        assert!(
            detail.contains(&id.to_string()),
            "the referencing plans are enumerated: {detail}"
        );
    }
}

/// The legitimate route D-54 leaves open: remove the inbound edges first, and
/// the rank may then go.
#[test]
fn a_rank_less_republish_with_no_inbound_edges_publishes() {
    let shape = shape_with(PlanChangeContract::default());

    assert!(codes(&run(&graph(index(&[], &[])), &shape)).is_empty());
}

/// A plan that still carries its rank may be pointed at by anyone.
#[test]
fn keeping_the_rank_satisfies_the_reverse_guard() {
    let shape = shape_with(PlanChangeContract {
        allowed_change_targets: None,
        comparability_rank: Some(3),
        usage_counter_on_plan_change: UsageCounterOnPlanChange::Reset,
    });

    assert!(codes(&run(&graph(index(&[], &[other(7)])), &shape)).is_empty());
}

/// Every offending edge is reported, not the first.
#[test]
fn every_dangling_target_is_named() {
    let shape = shape_with(PlanChangeContract {
        allowed_change_targets: Some(vec![other(1), other(2)]),
        comparability_rank: Some(10),
        usage_counter_on_plan_change: UsageCounterOnPlanChange::Reset,
    });

    // The **codes**, not merely the count. A count assertion cannot tell
    // `CHANGE_TARGET_UNPUBLISHED` from the `COMPARABILITY_RANK_REQUIRED` an
    // unpublished target also produces -- measured by probing the dangling arm
    // and watching this case stay green while its sibling reddened.
    assert_eq!(
        codes(&run(&graph(index(&[], &[])), &shape)),
        [CHANGE_TARGET_UNPUBLISHED, CHANGE_TARGET_UNPUBLISHED],
        "two dangling edges are two findings, both of them this code"
    );
}

// ---------------------------------------------------------------------------
// `inst-gs-perphase` / `inst-gs-shape` (§3 Entitlement Grant Set, §5, D-41, D-19).
// ---------------------------------------------------------------------------

fn phase(n: u128, ordinal: i32, converts_to: Option<u128>) -> PlanPhase {
    PlanPhase {
        phase_id: PhaseId::new(Uuid::from_u128(n)),
        kind: if converts_to.is_some() {
            PhaseKind::Trial
        } else {
            PhaseKind::Evergreen
        },
        display_name: None,
        ordinal,
        converts_to_phase_id: converts_to.map(|c| PhaseId::new(Uuid::from_u128(c))),
        phase_duration_days: converts_to.map(|_| 14),
        display_trial_days: None,
    }
}

fn quota(key: &str, value: i64) -> GrantSet {
    GrantSet {
        feature_flags: BTreeMap::new(),
        quotas: [(key.to_owned(), value)].into_iter().collect(),
    }
}

/// A plan with `schedule` and `grants`.
fn granted(schedule: Vec<PlanPhase>, grants: EntitlementGrants) -> PlanShape {
    let mut shape = PlanShape::new(plan(), 1, now());
    shape.phases = PhaseGraph::new(schedule);
    shape.entitlement_grants = grants;
    shape
}

/// The two-phase schedule D-41's own example uses: a trial converting to
/// evergreen.
fn phased() -> Vec<PlanPhase> {
    vec![phase(0xa1, 0, Some(0xa2)), phase(0xa2, 1, None)]
}

#[test]
fn the_grant_phase_code_is_the_designs_verbatim() {
    assert_eq!(GRANT_SET_PHASE_UNKNOWN, "GRANT_SET_PHASE_UNKNOWN");
}

#[test]
fn a_per_phase_key_naming_no_phase_fails_publish_naming_the_key() {
    let grants = EntitlementGrants {
        per_phase: [(Uuid::from_u128(0xdead), quota("cloudlets", 20))]
            .into_iter()
            .collect(),
        ..EntitlementGrants::default()
    };

    let report = run(&GrantSetPhasesKnown, &granted(phased(), grants));

    assert_eq!(codes(&report), [GRANT_SET_PHASE_UNKNOWN]);
    assert!(
        report.violations[0]
            .detail
            .contains(&Uuid::from_u128(0xdead).to_string()),
        "{}",
        report.violations[0].detail
    );
}

/// D-41's own scenario: tighter quotas in the trial than in the evergreen phase.
#[test]
fn a_per_phase_entry_on_a_phase_of_the_schedule_publishes() {
    let grants = EntitlementGrants {
        plan_level: quota("cloudlets", 1000),
        per_phase: [(Uuid::from_u128(0xa1), quota("cloudlets", 20))]
            .into_iter()
            .collect(),
        ..EntitlementGrants::default()
    };

    assert!(codes(&run(&GrantSetPhasesKnown, &granted(phased(), grants))).is_empty());
}

/// D-19: a plan whose schedule is only the implicit terminal phase is
/// **non-phased**, and an entry keyed to that sole phase fails too — it must be
/// authored as the plan-level set, or the plan publishes one fact twice with
/// nothing saying which wins.
#[test]
fn any_per_phase_entry_on_a_non_phased_plan_fails_including_one_on_its_own_phase() {
    let sole = vec![phase(0xa1, 0, None)];
    let grants = EntitlementGrants {
        per_phase: [(Uuid::from_u128(0xa1), quota("cloudlets", 20))]
            .into_iter()
            .collect(),
        ..EntitlementGrants::default()
    };

    let report = run(&GrantSetPhasesKnown, &granted(sole, grants));

    assert_eq!(codes(&report), [GRANT_SET_PHASE_UNKNOWN]);
    assert!(
        report.violations[0].detail.contains("non-phased"),
        "the refusal says which of the two it is: {}",
        report.violations[0].detail
    );
}

#[test]
fn a_plan_authoring_no_per_phase_entries_is_not_this_rules_business() {
    for schedule in [vec![phase(0xa1, 0, None)], phased()] {
        let grants = EntitlementGrants {
            plan_level: quota("cloudlets", 1000),
            ..EntitlementGrants::default()
        };
        assert!(codes(&run(&GrantSetPhasesKnown, &granted(schedule, grants))).is_empty());
    }
}

#[test]
fn every_dangling_phase_key_is_reported() {
    let grants = EntitlementGrants {
        per_phase: [
            (Uuid::from_u128(0xdead), quota("a", 1)),
            (Uuid::from_u128(0xbeef), quota("b", 2)),
        ]
        .into_iter()
        .collect(),
        ..EntitlementGrants::default()
    };

    assert_eq!(
        codes(&run(&GrantSetPhasesKnown, &granted(phased(), grants))),
        [GRANT_SET_PHASE_UNKNOWN, GRANT_SET_PHASE_UNKNOWN]
    );
}

// ---------------------------------------------------------------------------
// The materialized `phase -> grant-set` map (`inst-gs-perphase`, `inst-gs-resolved`).
// ---------------------------------------------------------------------------

/// Every phase maps to its effective set: the authored entry where one exists,
/// the plan-level set everywhere else. Subscriptions then resolves the active
/// phase at `t` by one lookup and never merges fallbacks at runtime.
#[test]
fn the_map_is_complete_and_falls_back_to_the_plan_level_set() {
    let grants = EntitlementGrants {
        plan_level: quota("cloudlets", 1000),
        per_phase: [(Uuid::from_u128(0xa1), quota("cloudlets", 20))]
            .into_iter()
            .collect(),
        ..EntitlementGrants::default()
    };

    let map = grants.phase_map(&phased());

    assert_eq!(map.len(), 2, "every phase of the schedule is present");
    assert_eq!(map[&Uuid::from_u128(0xa1)], quota("cloudlets", 20));
    assert_eq!(
        map[&Uuid::from_u128(0xa2)],
        quota("cloudlets", 1000),
        "the phase with no entry of its own resolves the plan-level set"
    );
}

/// An unphased plan publishes an empty map, not a one-entry one: its grant set
/// **is** the plan-level set, already published beside this, and a synthetic key
/// would invite a consumer to key off a phase D-19 makes implicit.
#[test]
fn an_unphased_plan_publishes_no_map_rather_than_a_one_entry_one() {
    let grants = EntitlementGrants {
        plan_level: quota("cloudlets", 1000),
        ..EntitlementGrants::default()
    };

    assert!(grants.phase_map(&[phase(0xa1, 0, None)]).is_empty());
}

// ---------------------------------------------------------------------------
// The K2 roster (Z13-3)
// ---------------------------------------------------------------------------

/// **Adding a `BillingAnchorPolicy` member without adding it to `ALL` does not
/// compile.**
///
/// This is the only kind of check that can catch a missing roster entry: a length
/// assertion is satisfied by any array of the right size, and a token comparison
/// ranges over the roster and so cannot see what the roster omits. The exhaustive
/// `match` below has no wildcard, so a fourth K2 policy is a compile error here —
/// which is what the two hand-written token matches this roster replaced could not
/// be, and why a fourth policy would have been a legal request refused as unknown
/// (`api::rest::prices`) and a stored row refusing to load (`price_repo`).
#[test]
fn every_billing_anchor_policy_member_is_in_the_roster() {
    fn rostered(policy: BillingAnchorPolicy) -> bool {
        // No `_ =>` arm, deliberately: this match is the gate.
        match policy {
            BillingAnchorPolicy::CalendarMonth
            | BillingAnchorPolicy::SubscriptionStart
            | BillingAnchorPolicy::FixedDay(_) => BillingAnchorPolicy::ALL
                .iter()
                .any(|member| member.as_str() == policy.as_str()),
        }
    }

    let day = AnchorDay::new(17).expect("17 is a day a month can have");
    for policy in [
        BillingAnchorPolicy::CalendarMonth,
        BillingAnchorPolicy::SubscriptionStart,
        BillingAnchorPolicy::FixedDay(day),
    ] {
        assert!(
            rostered(policy),
            "{policy} is not in BillingAnchorPolicy::ALL"
        );
    }
}

/// **One token per member, so a parse over the roster is unambiguous.**
///
/// `ALL` is a parse roster, and the readers walk it comparing `as_str`. Two members
/// rendering one token would make the first one found win silently — and it is a
/// live hazard here rather than a hypothetical, because `FixedDay` renders
/// `fixed_day` for all 31 days: a roster that listed the days would answer one
/// token 31 times, which is why it lists the policy once on a placeholder day.
#[test]
fn the_roster_renders_one_token_per_member() {
    let tokens: std::collections::BTreeSet<&str> = BillingAnchorPolicy::ALL
        .iter()
        .map(|policy| policy.as_str())
        .collect();

    assert_eq!(
        tokens.len(),
        BillingAnchorPolicy::ALL.len(),
        "two roster members render one token, so a parse over ALL is ambiguous: {tokens:?}"
    );
}

/// **The roster's `FixedDay` placeholder never escapes a parse.**
///
/// `with_anchor_day` is what re-pairs the day after a token is read back through
/// `ALL`, and the placeholder day would otherwise be a default nobody chose. The
/// converse arm matters as much: a day paired onto a policy that anchors without
/// one is **ignored** rather than absorbed, because absorbing it would make the two
/// unpublishable spellings K2 forbids representable again — the refusal belongs to
/// the caller, which is the only place that knows whether a day was sent.
#[test]
fn pairing_a_day_replaces_the_placeholder_and_leaves_the_dayless_policies_alone() {
    let day = AnchorDay::new(29).expect("29 is a day a month can have");

    let fixed = BillingAnchorPolicy::ALL
        .iter()
        .find(|policy| policy.as_str() == "fixed_day")
        .copied()
        .expect("fixed_day is in the roster");
    assert_ne!(
        fixed.anchor_day(),
        Some(day),
        "the placeholder must not be the day under test, or this proves nothing"
    );
    assert_eq!(fixed.with_anchor_day(day).anchor_day(), Some(day));

    for dayless in [
        BillingAnchorPolicy::CalendarMonth,
        BillingAnchorPolicy::SubscriptionStart,
    ] {
        assert_eq!(
            dayless.with_anchor_day(day),
            dayless,
            "{dayless} anchors without a day and must not silently acquire one"
        );
    }
}

// ---------------------------------------------------------------------------
// The pipeline's own wire — that each rule above is *registered*, not merely
// correct.
// ---------------------------------------------------------------------------

/// **Every rule `consumer_contract_rules` registers is reached through it.**
///
/// Every case above drives a rule with `run(&Rule, &shape)` — the rule directly,
/// bypassing the pipeline. That covers each `evaluate` body and covers the wire
/// into [`consumer_contract_rules`] with nothing: deleting any one
/// `.with_rule(..)` line left `cargo test -p cf-gears-bss-pricing --lib` at 1530 passed /
/// 0 failed, which is a feature shipped inert while its code sits in the catalog
/// looking enforced. `PRORATION_INPUTS_MISSING` appeared in no file under
/// `tests/` for exactly that reason — nothing exercised the door, only the room
/// behind it.
///
/// Each shape here is one an existing case above already proves trips exactly its
/// own rule; what is new is that the report comes from the pipeline. The
/// assertion is `contains` rather than an equality on the code list, so a shape
/// that also trips a sibling still names its own rule and a removed registration
/// reddens exactly the row it removed.
///
/// A rule registered later with no row here is the gap this test exists to close,
/// so the count is asserted against the pipeline's own length rather than left to
/// a reader.
#[test]
fn every_rule_this_pipeline_registers_is_reached_through_it() {
    let empty = ChangeTargetIndex::empty();

    // `inst-pi-uniform`: one market, two rows, contracts differing in one member.
    let mixed_market = shape_of(vec![
        priced(
            0xe4,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            Some(baseline_contract()),
        ),
        priced(
            0xe5,
            "EUR",
            "eu",
            PriceEligibility::AllSubscriptions,
            Some(contract(
                BillingAnchorPolicy::SubscriptionStart,
                ProrationBasis::CalendarDaysActual,
                false,
            )),
        ),
    ]);

    // `inst-pc-targets`: an edge to a plan the index does not carry as published.
    let dangling_target = shape_with(PlanChangeContract {
        allowed_change_targets: Some(vec![other(1)]),
        comparability_rank: Some(10),
        usage_counter_on_plan_change: UsageCounterOnPlanChange::Reset,
    });

    // D-41: a per-phase grant key naming a phase the schedule does not have.
    let unknown_phase = granted(
        phased(),
        EntitlementGrants {
            per_phase: [(Uuid::from_u128(0xdead), quota("cloudlets", 20))]
                .into_iter()
                .collect(),
            ..EntitlementGrants::default()
        },
    );

    // Keyed by the rule's own `name()` — the instruction id — and in registration
    // order, so the roster assertion below is a census of the pipeline rather than
    // a count of this array.
    let cases: [(&str, &str, &PlanShape); 6] = [
        (
            "inst-bt-required",
            BILLING_TIMING_MISSING,
            &shape_of(vec![row(
                0xe1,
                ChargeKind::Recurring,
                "EUR",
                "eu",
                PriceEligibility::AllSubscriptions,
                None,
                None,
            )]),
        ),
        (
            "inst-pi-required",
            PRORATION_INPUTS_MISSING,
            &shape_of(vec![priced(
                0xe2,
                "EUR",
                "eu",
                PriceEligibility::AllSubscriptions,
                None,
            )]),
        ),
        (
            "inst-pi-credit-none",
            PRORATION_INPUTS_CONTRADICTORY,
            &shape_of(vec![priced(
                0xe3,
                "EUR",
                "eu",
                PriceEligibility::AllSubscriptions,
                Some(contract(
                    BillingAnchorPolicy::CalendarMonth,
                    ProrationBasis::None,
                    true,
                )),
            )]),
        ),
        (
            "inst-pi-uniform",
            PRORATION_CONTRACT_MIXED_MARKET,
            &mixed_market,
        ),
        (
            "inst-pc-targets",
            CHANGE_TARGET_UNPUBLISHED,
            &dangling_target,
        ),
        ("inst-gs-perphase", GRANT_SET_PHASE_UNKNOWN, &unknown_phase),
    ];

    // The behavioural half first: on a removed `.with_rule` this is the assertion
    // that reddens, and it reddens naming the instruction whose door is gone.
    for (rule, code, shape) in cases {
        let report = consumer_contract_rules(&empty).run(shape);
        assert!(
            codes(&report).iter().any(|found| found == code),
            "{rule} did not answer {code} through the pipeline on a shape that \
             trips it when the rule is driven directly - it is either not \
             registered or no longer fires; the report was {:?}",
            codes(&report)
        );
    }

    // And the roster half, which catches the other direction: a rule registered
    // later with no row above, whose wire would then be as unproven as this one's
    // was.
    assert_eq!(
        consumer_contract_rules(&empty).rule_names(),
        cases.iter().map(|&(rule, ..)| rule).collect::<Vec<_>>(),
        "a rule registered with no row here has an unproven wire, and a row for a \
         rule nobody registers proves nothing"
    );
}
