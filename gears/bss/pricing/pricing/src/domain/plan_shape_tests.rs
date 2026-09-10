//! Tests for the Slice-2 plan-shape vocabulary.
//!
//! Two kinds of assertion, and neither is a derive. The token tests pin the
//! persisted spelling of every enum: the storage boundary writes an inverse of
//! `as_str()`, so a renamed token is a column value no reader recognises, and a
//! token naming two variants is an inverse that cannot exist. The derivation
//! tests pin the four `PhaseGraph` readings and the market unions, which are
//! what nine later rules share — if `entry()` ever answered "first authored"
//! instead of "lowest ordinal", setup timing and migration entry would land on
//! different phases and both would still look right.

use std::fmt;

use uuid::Uuid;

use super::{
    BillingCycle, CustomIntervalUnit, Frequency, PhaseGraph, PhaseKind, PlanPhase, PlanShape,
};
use crate::domain::concurrency::RowVersion;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::CurrencyCode;
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{ModelKind, PriceRow};
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
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

fn currency(code: &str) -> CurrencyCode {
    CurrencyCode::new(code).expect("test currency is three letters")
}

fn region(value: &str) -> Region {
    Region::new(value).expect("test region is non-blank")
}

fn now() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, 12, 0, 0)
}

fn phase(seed: u128, kind: PhaseKind, ordinal: i32, converts_to: Option<PhaseId>) -> PlanPhase {
    PlanPhase {
        phase_id: phase_id(seed),
        kind,
        display_name: None,
        ordinal,
        converts_to_phase_id: converts_to,
        phase_duration_days: converts_to.is_some().then_some(14),
        display_trial_days: None,
    }
}

fn record(charge_kind: ChargeKind, code: &str, market: &str, on_phase: PhaseId) -> PriceRecord {
    let scope_key = ScopeKey::new(
        plan(),
        currency(code),
        region(market),
        on_phase,
        PriceEligibility::AllSubscriptions,
        charge_kind,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none");

    PriceRecord {
        price_id: Uuid::from_u128(0xb_1000),
        scope_key,
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

fn shape() -> PlanShape {
    PlanShape::new(plan(), 3, now())
}

/// Round-trip every member of an `ALL` list through its token and back.
///
/// This is the test the storage lane leans on. The domain owns the variant
/// list; a repository turns a stored token into a value by walking it. So what
/// has to hold is that the walk is *total* (every member renders a token that
/// finds it again) and *injective* (no token finds two). A variant added later
/// and left out of `ALL` fails the count assertion at each call site; a variant
/// added to `ALL` whose token collides with another fails here.
fn round_trip<T: Copy + PartialEq + fmt::Debug>(variants: &[T], token: impl Fn(T) -> &'static str) {
    for &variant in variants {
        let rendered = token(variant);
        let mut matched = variants
            .iter()
            .copied()
            .filter(|other| token(*other) == rendered);
        let first = matched
            .next()
            .expect("a variant always matches its own token");
        assert_eq!(first, variant, "{rendered} resolves to a different variant");
        assert!(
            matched.next().is_none(),
            "the token {rendered} names more than one variant"
        );
    }
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

#[test]
fn billing_cycle_tokens_are_the_persisted_spelling() {
    assert_eq!(BillingCycle::OneTime.as_str(), "one_time");
    assert_eq!(BillingCycle::Recurring.as_str(), "recurring");
    assert_eq!(BillingCycle::Usage.as_str(), "usage");
    assert_eq!(BillingCycle::Hybrid.as_str(), "hybrid");

    assert_eq!(BillingCycle::ALL.len(), 4, "one member per variant");
    round_trip(BillingCycle::ALL, BillingCycle::as_str);
    for cycle in BillingCycle::ALL {
        assert_eq!(cycle.to_string(), cycle.as_str());
    }
}

#[test]
fn custom_interval_unit_tokens_are_the_persisted_spelling() {
    assert_eq!(CustomIntervalUnit::Days.as_str(), "days");
    assert_eq!(CustomIntervalUnit::Months.as_str(), "months");

    assert_eq!(CustomIntervalUnit::ALL.len(), 2, "one member per variant");
    round_trip(CustomIntervalUnit::ALL, CustomIntervalUnit::as_str);
    for unit in CustomIntervalUnit::ALL {
        assert_eq!(unit.to_string(), unit.as_str());
    }
}

#[test]
fn frequency_tokens_are_the_persisted_spelling() {
    assert_eq!(Frequency::Monthly.as_str(), "monthly");
    assert_eq!(Frequency::Quarterly.as_str(), "quarterly");
    assert_eq!(Frequency::Semiannual.as_str(), "semiannual");
    assert_eq!(Frequency::Annual.as_str(), "annual");

    // The custom variant renders the bare discriminator, whatever its payload:
    // `n` and `unit` are their own columns, so a token that folded them in
    // would give the `frequency` column two spellings of one value.
    assert_eq!(
        Frequency::CustomEveryN {
            n: 30,
            unit: CustomIntervalUnit::Days,
        }
        .as_str(),
        "custom_every_n"
    );
    assert_eq!(
        Frequency::CustomEveryN {
            n: 2,
            unit: CustomIntervalUnit::Months,
        }
        .as_str(),
        "custom_every_n"
    );

    assert_eq!(Frequency::ALL.len(), 5, "one member per variant");
    round_trip(Frequency::ALL, Frequency::as_str);
    for frequency in Frequency::ALL {
        assert_eq!(frequency.to_string(), frequency.as_str());
    }
}

#[test]
fn the_custom_member_of_the_variant_list_carries_a_placeholder_interval() {
    // The list answers "what values exist". For the one variant whose payload
    // lives in two sibling columns it cannot also answer "what interval", so a
    // reader that matches a stored `custom_every_n` against `ALL` has to
    // rebuild `n` and `unit` from `custom_interval_n` / `custom_interval_unit`.
    // This test is what makes that contract visible rather than folklore.
    let custom = Frequency::ALL
        .iter()
        .copied()
        .find(|frequency| frequency.as_str() == "custom_every_n")
        .expect("the variant list covers the custom frequency");

    assert_eq!(
        custom,
        Frequency::CustomEveryN {
            n: Frequency::CUSTOM_INTERVAL_PLACEHOLDER,
            unit: CustomIntervalUnit::Days,
        }
    );
    assert_eq!(Frequency::CUSTOM_INTERVAL_PLACEHOLDER, 1);
}

#[test]
fn phase_kind_tokens_are_the_persisted_spelling() {
    assert_eq!(PhaseKind::Trial.as_str(), "trial");
    // `interim`, not `intro`, since D-358: the slot is "any time-boxed non-trial
    // phase" and the old word promised a business meaning nothing behaved on.
    assert_eq!(PhaseKind::Interim.as_str(), "interim");
    assert_eq!(PhaseKind::Evergreen.as_str(), "evergreen");
    assert_eq!(
        PhaseKind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>(),
        ["trial", "interim", "evergreen"],
        "the roster, in its stable order, and no `intro` in it"
    );

    assert_eq!(PhaseKind::ALL.len(), 3, "one member per variant");
    round_trip(PhaseKind::ALL, PhaseKind::as_str);
    for kind in PhaseKind::ALL {
        assert_eq!(kind.to_string(), kind.as_str());
    }
}

// `only_the_custom_frequency_carries_an_interval` stood here and was deleted on
// 2026-08-20. It built a `Frequency::CustomEveryN { n: 45, unit: Days }`,
// destructured it two lines later and asserted `n == 45` and `unit == Days` — a
// constructor echo over a compiler-guaranteed property. No change to production
// code could turn it red: a variant that stopped carrying the payload would fail
// to compile rather than fail the assertion, so it added coverage without adding
// a check. The property it claimed to hold — that the three-column
// decomposition's two illegal states are unrepresentable — is the type's, stated
// on `Frequency` itself, and the place it *could* fail is the storage boundary
// that reassembles `frequency` / `custom_interval_n` / `custom_interval_unit`,
// which is `infra`'s and has no assertion here to lose.

// ---------------------------------------------------------------------------
// The cycle predicates
// ---------------------------------------------------------------------------

#[test]
fn the_three_cycle_predicates_answer_for_every_cycle() {
    // Each column is one rule's scope, named for the rule that reads it. The
    // whole matrix is here so that widening a predicate to a third cycle is a
    // visible edit rather than a rule that quietly starts firing.
    for (cycle, recurring, usage, setup) in [
        (BillingCycle::OneTime, false, false, false),
        (BillingCycle::Recurring, true, false, true),
        (BillingCycle::Usage, false, true, false),
        (BillingCycle::Hybrid, true, true, true),
    ] {
        assert_eq!(
            cycle.has_recurring_part(),
            recurring,
            "has_recurring_part on {cycle}"
        );
        assert_eq!(
            cycle.requires_usage_part(),
            usage,
            "requires_usage_part on {cycle}"
        );
        assert_eq!(
            cycle.admits_setup_row(),
            setup,
            "admits_setup_row on {cycle}"
        );
    }
}

// ---------------------------------------------------------------------------
// The phase graph
// ---------------------------------------------------------------------------

#[test]
fn terminality_is_structural_and_never_the_kind() {
    // C-4: the two were conflated once. The type must still be able to hold
    // both illegal combinations, because `TERMINAL_PHASE_KIND_INVALID` has
    // nothing to reject otherwise.
    let evergreen_with_successor = phase(1, PhaseKind::Evergreen, 0, Some(phase_id(2)));
    let trial_without_successor = phase(3, PhaseKind::Trial, 1, None);

    assert!(!evergreen_with_successor.is_terminal());
    assert!(trial_without_successor.is_terminal());
}

#[test]
fn terminals_reports_zero_one_and_two_rather_than_collapsing_them() {
    // Zero and two are exactly the states `PHASE_GRAPH_INVALID` reports, and an
    // `Option` return renders both as `None` or as a first-wins pick.
    let none = PhaseGraph::new(vec![
        phase(1, PhaseKind::Trial, 0, Some(phase_id(2))),
        phase(2, PhaseKind::Evergreen, 1, Some(phase_id(1))),
    ]);
    assert!(none.terminals().is_empty());

    let one = PhaseGraph::new(vec![
        phase(1, PhaseKind::Trial, 0, Some(phase_id(2))),
        phase(2, PhaseKind::Evergreen, 1, None),
    ]);
    assert_eq!(one.terminals().len(), 1);
    assert_eq!(one.terminals()[0].phase_id, phase_id(2));

    let two = PhaseGraph::new(vec![
        phase(1, PhaseKind::Evergreen, 0, None),
        phase(2, PhaseKind::Evergreen, 1, None),
    ]);
    assert_eq!(two.terminals().len(), 2);
}

#[test]
fn entry_is_the_lowest_ordinal_and_not_the_first_authored() {
    // No column records authoring order, and D-39's "first non-trial phase"
    // plus `inst-cs-setup-timing`'s both read the ordinal order — so a reading
    // that answered "first authored" would put setup timing and migration
    // entry on different phases.
    let graph = PhaseGraph::new(vec![
        phase(20, PhaseKind::Evergreen, 2, None),
        phase(10, PhaseKind::Trial, 0, Some(phase_id(15))),
        phase(15, PhaseKind::Interim, 1, Some(phase_id(20))),
    ]);

    assert_eq!(
        graph.entry().map(|entry| entry.phase_id),
        Some(phase_id(10))
    );
    assert_eq!(
        graph
            .in_ordinal_order()
            .iter()
            .map(|item| item.phase_id)
            .collect::<Vec<_>>(),
        vec![phase_id(10), phase_id(15), phase_id(20)]
    );
    assert_eq!(
        graph.phases()[0].phase_id,
        phase_id(20),
        "the authored order is preserved and is not the ordinal order"
    );
}

#[test]
fn an_empty_graph_has_no_entry_and_no_terminal() {
    let graph = PhaseGraph::default();

    assert!(graph.entry().is_none());
    assert!(graph.terminals().is_empty());
    assert!(graph.phases().is_empty());
    assert!(graph.find(phase_id(1)).is_none());
}

#[test]
fn find_locates_a_phase_and_answers_none_for_a_dangling_reference() {
    let graph = PhaseGraph::new(vec![phase(7, PhaseKind::Evergreen, 0, None)]);

    assert_eq!(
        graph.find(phase_id(7)).map(|found| found.kind),
        Some(PhaseKind::Evergreen)
    );
    assert!(graph.find(phase_id(8)).is_none());
}

// ---------------------------------------------------------------------------
// The plan-shape derivations
// ---------------------------------------------------------------------------

#[test]
fn markets_is_the_de_duplicated_union_over_the_candidate_rows() {
    let terminal = phase_id(9);
    let mut subject = shape();
    subject.rows = vec![
        record(ChargeKind::Recurring, "usd", "US", terminal),
        record(ChargeKind::Usage, "usd", "US", terminal),
        record(ChargeKind::Recurring, "eur", "EU", terminal),
        // The same market spelled in lower case, on another phase. The currency
        // axis normalizes, so this is still one market and not a third.
        record(ChargeKind::Recurring, "USD", "US", phase_id(8)),
    ];

    let markets = subject.markets();

    assert_eq!(markets.len(), 2);
    assert!(markets.contains(&(currency("USD"), region("US"))));
    assert!(markets.contains(&(currency("EUR"), region("EU"))));
}

#[test]
fn markets_with_selects_one_charge_kind() {
    // D-84 is a statement about two of these sets: every market carrying a
    // recurring row must also carry the plan's usage lines.
    let terminal = phase_id(9);
    let mut subject = shape();
    subject.rows = vec![
        record(ChargeKind::Recurring, "usd", "US", terminal),
        record(ChargeKind::Recurring, "eur", "EU", terminal),
        record(ChargeKind::Usage, "eur", "EU", terminal),
    ];

    let recurring = subject.markets_with(ChargeKind::Recurring);
    let usage = subject.markets_with(ChargeKind::Usage);

    assert_eq!(recurring.len(), 2);
    assert_eq!(usage, [(currency("EUR"), region("EU"))].into());
    assert_eq!(
        recurring.difference(&usage).count(),
        1,
        "the USD market prices no usage line, which is what D-84 reports"
    );
}

#[test]
fn usage_rows_and_rows_on_phase_read_the_scope_key() {
    let terminal = phase_id(9);
    let trial = phase_id(8);
    let mut subject = shape();
    subject.rows = vec![
        record(ChargeKind::Recurring, "usd", "US", terminal),
        record(ChargeKind::Usage, "usd", "US", terminal),
        record(ChargeKind::Usage, "usd", "US", trial),
        record(ChargeKind::OneTimeSetup, "usd", "US", terminal),
    ];

    assert_eq!(subject.usage_rows().len(), 2);
    assert_eq!(subject.rows_on_phase(trial).len(), 1);
    assert_eq!(subject.rows_on_phase(terminal).len(), 3);
    assert!(subject.rows_on_phase(phase_id(404)).is_empty());
}

#[test]
fn a_fresh_shape_carries_nothing_but_its_name_and_its_clock() {
    // A plan is authored incrementally while it is `draft`, so the unfinished
    // state has to be representable — and `evaluated_at` is a parameter rather
    // than a `Default`, because a default would be the Unix epoch and every
    // availability comparison against 1970 answers confidently and wrongly.
    let subject = shape();

    assert_eq!(subject.plan_id, plan());
    assert_eq!(subject.revision, 3);
    assert_eq!(subject.evaluated_at, now());
    assert!(subject.billing_cycle.is_none());
    assert!(subject.frequency.is_none());
    assert!(subject.plan_tier.is_none());
    assert!(!subject.plan_tier_override);
    assert!(subject.available_from.is_none());
    assert!(subject.available_to.is_none());
    assert!(subject.purchase_min_qty.is_none());
    assert!(subject.purchase_max_qty.is_none());
    assert!(subject.invoice_grouping_key.is_none());
    assert!(subject.descriptor_set.is_none());
    assert!(subject.baseline.is_none());
    assert!(subject.addon_rules.is_empty());
    assert!(subject.rows.is_empty());
    assert!(subject.phases.phases().is_empty());
    assert_eq!(subject.subject(), format!("{}/3", plan()));
}

// A constructor echo has no home here: assigning `PlanShape`'s plain `pub` fields
// and reading them back runs no logic, and a field assignment cannot fail. The
// only thing that can drop a member is the storage round trip that reassembles
// it, which `infra` owns and which `tests/sqlite_plan_repo.rs` reads back whole.
