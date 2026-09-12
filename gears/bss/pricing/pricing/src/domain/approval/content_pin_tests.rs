//! What the content pin has to be, executed.
//!
//! Several kinds of assertion, and the first is the one that carries the weight.
//!
//! **Every hashed field moves the pin.** [`every_hashed_field_moves_the_pin`]
//! ranges over a mutator per field of every struct the encoder frames, over a
//! *maximal* base shape in which every `Option` is `Some` and every collection
//! is non-empty — so each mutator changes a value rather than filling in an
//! absence. It also asserts that no two mutators land on the **same** digest,
//! which is what catches a field boundary that can be moved: the failure mode of
//! an unframed encoding is not a hash that fails to move, it is two different
//! shapes that hash alike.
//!
//! **Two things are deliberately not hashed**, and they have their own tests
//! rather than an omission from the list above. Hashing `evaluated_at` would
//! make every approve answer `APPROVAL_CONTENT_MISMATCH` with nothing changed —
//! the failure is total, silent and looks like an attack — so it is pinned as an
//! exclusion, not left to the reader of the encoder.
//!
//! **Collections are sets, and their query order is not content.** A repository
//! `ORDER BY` is not something an author changed; if it moved the pin, the day
//! somebody re-orders a read is the day every open approval unit in the fleet
//! refuses.
//!
//! **A window's clock-driven state is not content either, and its cancellation
//! is.** [`the_clock_may_flip_a_window_but_not_the_pin`] is the pair, in one
//! place, because they are one rule read from two sides: D-99 gives activation and
//! expiry no projected consequence, and gives a cancellation every consequence a
//! publish unit has.
//!
//! # What cannot be tested here, stated rather than skipped
//!
//! - `ScopeKey::price_overlay` has exactly one value in this gear
//!   (`PriceOverlay::Base`), so no mutator can move it and its framing rests on
//!   inspection. It is framed anyway, so the day S9 adds an overlay the pin
//!   already covers the axis.
//! - The encoder's exhaustiveness is **not** a test's job and is not claimed to
//!   be one. Every struct is destructured with no `..` in `content_pin.rs`, so a
//!   new field is a compile error there. A test can only fail for the fields it
//!   already knows about.

use std::collections::{BTreeMap, BTreeSet};

use uuid::Uuid;

use super::{OVERLAY_PIN_DOMAIN_SEP, overlay_content_hash};
use super::{content_hash, threshold_content_hash};
use crate::domain::audit::hex32;
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{
    AnchorDay, BillingAnchorPolicy, GrantSet, ProrationBasis, ProrationContract,
    UsageCounterOnPlanChange,
};
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::materiality::{ThresholdBasis, ThresholdEntry, ThresholdVersion};
use crate::domain::money::{CurrencyCode, MinorAmount, RateMinor};
use crate::domain::overlay::{
    Adjustment, AmountSet, Disclosure, LineKey, Magnitude, OverlayInterval, OverlayLifecycle,
    OverlayLine, OverlayRevision, ScopeClass, ScopeSelector, ScopeValue, TargetRef, TargetSku,
    TaxBasis,
};
use crate::domain::plan_shape::{
    AddonRule, BillingCycle, CustomIntervalUnit, DescriptorSet, Frequency, PeriodFloorCap,
    PhaseGraph, PhaseKind, PlanPhase, PlanShape, PublishedBaseline,
};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{
    AggregationFunction, AggregationGranularity, BandTop, BillingGranularity, IncludedAllowance,
    MinQtyUsageFallback, ModelKind, PriceRow, QuantitySource, ReservationFlavor, RolloverPolicy,
    TierAggregationWindow, TierBand, TierQualificationWindow,
};
use crate::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::window::{KeyWindows, WindowInterval, WindowState};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// A maximal shape: every option present, every collection non-empty.
// ---------------------------------------------------------------------------

/// A band rate in whole minor units, scaled to the stored rate scale
/// (D-311) so these cases price what they always priced.
fn rate(minor_units: i64) -> RateMinor {
    RateMinor::from_minor_units(minor_units).expect("a non-negative rate")
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, hour, 0, 0)
}

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x91a4))
}

fn phase_id(seed: u128) -> PhaseId {
    PhaseId::new(Uuid::from_u128(seed))
}

fn money(units: i64) -> MinorAmount {
    MinorAmount::new(units).expect("a non-negative test amount")
}

fn key(charge_kind: ChargeKind, code: &str, market: &str, phase: PhaseId) -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new(code).expect("three letters"),
        Region::new(market).expect("a non-blank region"),
        phase,
        PriceEligibility::AllSubscriptions,
        charge_kind,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
}

/// A usage row with **every** Slice-3 field authored, so a mutator on any of
/// them changes a value rather than filling in a hole.
fn maximal_row() -> PriceRow {
    PriceRow {
        charge_kind: ChargeKind::Usage,
        model_kind: Some(ModelKind::Graduated),
        amount_minor: Some(money(500)),
        // Authored, like every other optional field here, so the mutator below
        // moves a value rather than filling a hole -- a pin that only proved
        // `None -> Some` would say nothing about a rate a reviewer actually read.
        // The row is `graduated` and would not publish carrying both, which this
        // fixture is right to ignore: it exercises the **encoding**, and an
        // encoding that only framed publishable rows would leave the field
        // unpinned on exactly the rows a `PATCH` is moving through.
        unit_rate: Some(rate(6)),
        bands: vec![
            TierBand::closed(0, 100, rate(10)),
            TierBand::open(100, rate(5)),
        ],
        package_size: Some(50),
        package_price_minor: Some(money(400)),
        quantity_source: Some(QuantitySource::Manual),
        manual_quantity: Some(7),
        meter: Some("api.calls".to_owned()),
        dimension_key: "region:eu".to_owned(),
        billing_granularity: Some(BillingGranularity::PerHour),
        tier_aggregation_window: Some(TierAggregationWindow::CalendarMonth),
        tier_qualification_window: Some(TierQualificationWindow::Current),
        aggregation_function: Some(AggregationFunction::Peak),
        aggregation_granularity: Some(AggregationGranularity::Hour),
        max_hold_granules: Some(3),
        included_allowance: Some(IncludedAllowance {
            quantity: 1_000,
            rollover_policy: RolloverPolicy::Carry,
        }),
        reserved_rate: Some(RateMinor::from_minor_units(250).expect("a non-negative rate")),
        reservation_flavor: Some(ReservationFlavor::Capacity),
        min_qty_purchase: Some(10),
        min_qty_usage: Some(20),
        min_qty_usage_fallback: Some(MinQtyUsageFallback::Exception),
        discount_ref: Some("promo/spring".to_owned()),
    }
}

fn maximal_record(seed: u128) -> PriceRecord {
    PriceRecord {
        price_id: Uuid::from_u128(seed),
        scope_key: key(ChargeKind::Usage, "USD", "EU", phase_id(0x11)),
        row: maximal_row(),
        tax_inclusive: true,
        // Authored, for `maximal_row`'s reason one function up: with `None` here
        // the only mutator available is a `None -> Some`, and an encoder writing
        // `put_bool(...is_some())` would satisfy it while leaving the *value* a
        // reviewer read unframed. From a value, the table below moves it to
        // another value and to absence, and only a framed token passes both.
        tax_category_ref: Some("standard".to_owned()),
        billing_timing: Some("advance".to_owned()),
        proration_contract: None,
        rounding_policy_ref: Some("half-up".to_owned()),
        grandfather_until: Some(at(20)),
        supersedes_price_id: Some(Uuid::from_u128(0xdead)),
        lifecycle_state: LifecycleState::Draft,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: at(9),
        row_version: RowVersion::new(4),
    }
}

/// The baseline key with a usage line attached — D-196's ninth and tenth axes.
fn line_key(meter: Option<&str>, dimension: &str) -> ScopeKey {
    key(ChargeKind::Usage, "USD", "EU", phase_id(0x11))
        .with_usage_line(
            meter.map(|value| Meter::new(value).expect("a non-blank meter")),
            DimensionKey::new(dimension),
        )
        .expect("a usage key carries its line")
}

/// One key's window set with **every** field of both structs authored: an
/// interval with a closed end, one with an open end, and two distinct states.
///
/// The two-interval shape is what lets a mutator move one interval without
/// changing the group's length, which is the framing question — a group of two
/// must not hash like a group of one carrying the concatenation.
fn maximal_window_group(phase: PhaseId, market: &str) -> KeyWindows {
    KeyWindows {
        scope_key: key(ChargeKind::Usage, "USD", market, phase),
        intervals: vec![
            WindowInterval::new(at(4), Some(at(8)), WindowState::Expired),
            WindowInterval::new(at(8), None, WindowState::Active),
        ],
    }
}

fn maximal_phase(seed: u128, ordinal: i32, converts_to: Option<PhaseId>) -> PlanPhase {
    PlanPhase {
        phase_id: phase_id(seed),
        kind: PhaseKind::Trial,
        // Non-empty, so the golden vector covers the label's **value** and not
        // only its presence (the `tax_category_ref` argument on the frozen test).
        display_name: Some("Onboarding".to_owned()),
        ordinal,
        converts_to_phase_id: converts_to,
        phase_duration_days: Some(14),
        display_trial_days: Some(14),
    }
}

fn maximal_rule(seed: u128) -> AddonRule {
    AddonRule {
        addon_sku_id: Uuid::from_u128(seed),
        required: true,
        min_qty: Some(1),
        max_qty: Some(9),
        step_qty: Some(2),
        price_override_ref: Some(Uuid::from_u128(0xbeef)),
        depends_on: vec![Uuid::from_u128(0x0a), Uuid::from_u128(0x0b)],
        conflicts_with: vec![Uuid::from_u128(0x0c)],
    }
}

/// One proration contract, spelled in one place so the table above moves
/// exactly one member per row.
fn contract(
    billing_anchor_policy: BillingAnchorPolicy,
    proration_basis: ProrationBasis,
    credit_on_downgrade: bool,
) -> ProrationContract {
    ProrationContract {
        billing_anchor_policy,
        proration_basis,
        credit_on_downgrade,
    }
}

/// One grant set with **both** maps non-empty and a `false` among the flags.
///
/// The `false` is the point: every flag mutator below moves a value rather than
/// filling an absence, and a set whose flags were all `true` could not tell
/// `put_bool(*on)` from a framed constant.
fn grant_set(flags: &[(&str, bool)], quotas: &[(&str, i64)]) -> GrantSet {
    GrantSet {
        feature_flags: flags
            .iter()
            .map(|(key, on)| ((*key).to_owned(), *on))
            .collect(),
        quotas: quotas
            .iter()
            .map(|(key, value)| ((*key).to_owned(), *value))
            .collect(),
    }
}

/// One market's period floor/cap.
fn bound(currency: &str, region: &str, floor: Option<i64>, cap: Option<i64>) -> PeriodFloorCap {
    PeriodFloorCap {
        currency: CurrencyCode::new(currency).expect("a three-letter code"),
        region: Region::new(region).expect("a non-blank region"),
        floor_minor: floor.map(|m| MinorAmount::new(m).expect("a non-negative amount")),
        cap_minor: cap.map(|m| MinorAmount::new(m).expect("a non-negative amount")),
    }
}

fn base() -> PlanShape {
    let mut shape = PlanShape::new(plan(), 3, at(12));
    // One composite, so the indexed mutators below have a member to move and the
    // frozen digest covers a non-empty set -- an empty one would frame a count of
    // zero and prove nothing about the members (D-259).
    shape.composites = vec![crate::domain::plan_shape::CompositeMeter {
        composite_id: Uuid::from_u128(0xc0_01),
        output_unit: "vm-hour".to_owned(),
        constituent_units: vec!["vcpu-hour".to_owned(), "ram-gb-hour".to_owned()],
        formula: serde_json::json!({ "op": "weighted_sum", "weights": [1, 1] }),
    }];
    shape.sku_id = Some(Uuid::from_u128(0x5_c1));
    shape.billing_cycle = Some(BillingCycle::Recurring);
    shape.frequency = Some(Frequency::CustomEveryN {
        n: 7,
        unit: CustomIntervalUnit::Days,
    });
    shape.plan_tier = Some("gold".to_owned());
    shape.plan_name = Some("Gold Plan".to_owned());
    shape.plan_tier_override = true;
    shape.available_from = Some(at(1));
    shape.available_to = Some(at(23));
    shape.purchase_min_qty = Some(1);
    shape.purchase_max_qty = Some(10);
    shape.invoice_grouping_key = Some("emea".to_owned());
    shape.phases = PhaseGraph::new(vec![
        maximal_phase(0x11, 0, Some(phase_id(0x12))),
        maximal_phase(0x12, 1, None),
    ]);
    shape.addon_rules = vec![maximal_rule(0x21), maximal_rule(0x22)];
    shape.descriptor_set = Some(DescriptorSet {
        invoice_line_template: Some("{plan} - {phase}".to_owned()),
        gl_code: Some("4000".to_owned()),
        itemization_rule: Some("per_charge".to_owned()),
        additional: BTreeMap::from([
            ("costCentre".to_owned(), "cc-1".to_owned()),
            ("segment".to_owned(), "smb".to_owned()),
        ]),
    });
    shape.rows = vec![maximal_record(0xb001), maximal_record(0xb002)];
    shape.windows = vec![
        maximal_window_group(phase_id(0x11), "EU"),
        maximal_window_group(phase_id(0x12), "EU"),
    ];
    // **Two markets with complementary bounds** (D-319). The golden vector below
    // covered an empty collection until 2026-08-20 -- a framed count of zero and
    // nothing else -- so `put_period_floor_cap`'s four members were reached by no
    // assertion in this file and its body could have been emptied with the whole
    // suite green. That is the exact hole the `v14` entry on
    // `the_encoding_is_frozen` claims to have closed: a reviewer approving a plan
    // with no minimum while the commit publishes one at $500 a period.
    //
    // Complementary because one entry cannot tell a renderer that frames the
    // first bound from one that frames the whole set, and a set with no `cap`
    // anywhere cannot tell `cap_minor` from a framed constant `None`.
    shape.period_floor_caps = vec![
        bound("USD", "us", Some(50_000), None),
        bound("EUR", "de", None, Some(900_000)),
    ];
    // **Maximal, so every mutator below changes a value rather than filling an
    // absence.** These three were left at `Default` until 2026-08-20, which made
    // each grant mutator's digest move fully explained by the framed *count* and
    // *key*: nothing exercised `put_grant_set`'s `put_bool(*on)` or
    // `put_i64(*value)` against a changed value, so dropping either from the
    // preimage kept the suite green -- the "trial capped at 20 cloudlets published
    // as 20 000" hole `put_entitlement_grants`' own doc names.
    shape.entitlement_grants.plan_tier_ref = Some("tier-gold".to_owned());
    shape.entitlement_grants.plan_level = grant_set(
        &[("audit", true), ("sso", false)],
        &[("cloudlets", 20), ("seats", 50)],
    );
    shape.entitlement_grants.per_phase = BTreeMap::from([(
        phase_id(0x11).get(),
        grant_set(&[("beta", true)], &[("cloudlets", 2)]),
    )]);
    shape.change_contract.allowed_change_targets =
        Some(vec![Uuid::from_u128(0xed_01), Uuid::from_u128(0xed_02)]);
    shape.change_contract.comparability_rank = Some(7);
    shape.baseline = Some(PublishedBaseline {
        terminal_phase_id: phase_id(0x12),
        phase_ids_in_use: BTreeSet::from([phase_id(0x11)]),
        available_from: Some(at(1)),
        available_to: Some(at(23)),
    });
    shape
}

/// One mutator per field the encoder frames.
///
/// Written as a list rather than as one test per field because the interesting
/// assertion is over the *whole* list — that no two of them collide — and
/// because a list is what a reader diffs against the encoder.
#[allow(
    clippy::type_complexity,
    reason = "a table of named mutators is exactly what this is"
)]
type Mutator = (&'static str, fn(&mut PlanShape));

/// The whole table. Split into four only because one function of ninety
/// entries trips the line-count lint; the split has no other meaning.
fn mutators() -> Vec<Mutator> {
    let mut all = plan_level_mutators();
    all.extend(child_mutators());
    all.extend(window_mutators());
    all.extend(row_mutators());
    all.extend(slice10_row_mutators());
    all.extend(plan_contract_mutators());
    all.extend(period_floor_cap_mutators());
    all
}

/// D-319's plan-level period floor/cap, member by member.
///
/// Its own function for [`plan_contract_mutators`]' stated reason: the split is
/// mechanical, to stay under the line-count lint, and means nothing else.
///
/// Without these rows the whole of `put_period_floor_cap` could be deleted and
/// this suite would stay green. `base()` carried no bound at all until
/// 2026-08-20, so the four framed members were reached by no assertion in this
/// file and the frozen vector covered a framed count of zero -- which is
/// `shape.composites[0].*`'s case after D-259, one child set over.
fn period_floor_cap_mutators() -> Vec<Mutator> {
    vec![
        ("shape.period_floor_caps[0].floor_minor", |s| {
            s.period_floor_caps[0].floor_minor = Some(money(60_000));
        }),
        ("shape.period_floor_caps[0].cap_minor", |s| {
            s.period_floor_caps[0].cap_minor = Some(money(70_000));
        }),
        // A dropped bound: the market keeps its entry and stops stating a
        // minimum, which is the move a `put_opt_i64` that framed `0` for `None`
        // could not tell from a floor of nothing.
        ("shape.period_floor_caps[0].floor_minor (dropped)", |s| {
            s.period_floor_caps[0].floor_minor = None;
        }),
        ("shape.period_floor_caps[0].currency", |s| {
            s.period_floor_caps[0].currency =
                CurrencyCode::new("GBP").expect("a three-letter code");
        }),
        ("shape.period_floor_caps[0].region", |s| {
            s.period_floor_caps[0].region = Region::new("ca").expect("a non-blank region");
        }),
        ("shape.period_floor_caps: added", |s| {
            s.period_floor_caps
                .push(bound("JPY", "jp", Some(1), Some(2)));
        }),
        ("shape.period_floor_caps: dropped", |s| {
            s.period_floor_caps.truncate(1);
        }),
    ]
}

fn plan_level_mutators() -> Vec<Mutator> {
    vec![
        // PlanShape
        ("plan_id", |s| {
            s.plan_id = PlanId::new(Uuid::from_u128(0x99));
        }),
        ("revision", |s| s.revision = 4),
        // The rebind. Its entry is the one in this table with a named window
        // behind it: an approve re-derives the shape and re-verifies the pin, so
        // a `sku_id` outside the digest is a plan that can be pointed at another
        // SKU between the reviewer's sign-off and the commit with every digest
        // equal.
        ("sku_id", |s| s.sku_id = Some(Uuid::from_u128(0x5_c2))),
        ("sku_id -> None", |s| s.sku_id = None),
        ("billing_cycle", |s| {
            s.billing_cycle = Some(BillingCycle::Hybrid);
        }),
        ("billing_cycle -> None", |s| s.billing_cycle = None),
        ("frequency kind", |s| {
            s.frequency = Some(Frequency::Monthly);
        }),
        ("frequency custom n", |s| {
            s.frequency = Some(Frequency::CustomEveryN {
                n: 90,
                unit: CustomIntervalUnit::Days,
            });
        }),
        ("frequency custom unit", |s| {
            s.frequency = Some(Frequency::CustomEveryN {
                n: 7,
                unit: CustomIntervalUnit::Months,
            });
        }),
        ("frequency -> None", |s| s.frequency = None),
        ("plan_tier", |s| s.plan_tier = Some("silver".to_owned())),
        ("plan_tier -> None", |s| s.plan_tier = None),
        ("plan_tier -> empty", |s| s.plan_tier = Some(String::new())),
        ("plan_name", |s| {
            s.plan_name = Some("Silver Plan".to_owned());
        }),
        ("plan_name -> None", |s| s.plan_name = None),
        ("plan_name -> empty", |s| s.plan_name = Some(String::new())),
        ("plan_tier_override", |s| s.plan_tier_override = false),
        ("available_from", |s| s.available_from = Some(at(2))),
        ("available_to", |s| s.available_to = Some(at(22))),
        ("purchase_min_qty", |s| s.purchase_min_qty = Some(2)),
        ("purchase_max_qty", |s| s.purchase_max_qty = Some(11)),
        ("invoice_grouping_key", |s| {
            s.invoice_grouping_key = Some("apac".to_owned());
        }),
    ]
}

fn child_mutators() -> Vec<Mutator> {
    vec![
        // PhaseGraph / PlanPhase
        ("phases: one dropped", |s| {
            s.phases = PhaseGraph::new(vec![maximal_phase(0x12, 1, None)]);
        }),
        ("phase.phase_id", |s| {
            s.phases = PhaseGraph::new(vec![
                maximal_phase(0x13, 0, Some(phase_id(0x12))),
                maximal_phase(0x12, 1, None),
            ]);
        }),
        ("phase.kind", |s| {
            let mut phases = s.phases.phases().to_vec();
            phases[0].kind = PhaseKind::Interim;
            s.phases = PhaseGraph::new(phases);
        }),
        ("phase.ordinal", |s| {
            let mut phases = s.phases.phases().to_vec();
            phases[0].ordinal = -1;
            s.phases = PhaseGraph::new(phases);
        }),
        ("phase.converts_to_phase_id", |s| {
            let mut phases = s.phases.phases().to_vec();
            phases[0].converts_to_phase_id = Some(phase_id(0x14));
            s.phases = PhaseGraph::new(phases);
        }),
        ("phase.phase_duration_days", |s| {
            let mut phases = s.phases.phases().to_vec();
            phases[0].phase_duration_days = Some(90);
            s.phases = PhaseGraph::new(phases);
        }),
        ("phase.display_trial_days", |s| {
            let mut phases = s.phases.phases().to_vec();
            phases[0].display_trial_days = Some(30);
            s.phases = PhaseGraph::new(phases);
        }),
        // D-357: the label is framed, and all three transitions move the pin —
        // another value, absence, and the empty string (which the REST parser
        // never produces but the encoder must still tell from absence).
        ("phase.display_name", |s| {
            let mut phases = s.phases.phases().to_vec();
            phases[0].display_name = Some("Ramp".to_owned());
            s.phases = PhaseGraph::new(phases);
        }),
        ("phase.display_name -> None", |s| {
            let mut phases = s.phases.phases().to_vec();
            phases[0].display_name = None;
            s.phases = PhaseGraph::new(phases);
        }),
        ("phase.display_name -> empty", |s| {
            let mut phases = s.phases.phases().to_vec();
            phases[0].display_name = Some(String::new());
            s.phases = PhaseGraph::new(phases);
        }),
        // AddonRule
        ("addon_rules: one dropped", |s| {
            s.addon_rules = vec![maximal_rule(0x21)];
        }),
        ("rule.addon_sku_id", |s| {
            s.addon_rules[0].addon_sku_id = Uuid::from_u128(0x23);
        }),
        ("rule.required", |s| s.addon_rules[0].required = false),
        ("rule.min_qty", |s| s.addon_rules[0].min_qty = Some(3)),
        ("rule.max_qty", |s| s.addon_rules[0].max_qty = Some(8)),
        ("rule.step_qty", |s| s.addon_rules[0].step_qty = Some(3)),
        ("rule.price_override_ref", |s| {
            s.addon_rules[0].price_override_ref = Some(Uuid::from_u128(0xcafe));
        }),
        ("rule.depends_on", |s| {
            s.addon_rules[0].depends_on = vec![Uuid::from_u128(0x0a)];
        }),
        ("rule.conflicts_with", |s| {
            s.addon_rules[0].conflicts_with = vec![Uuid::from_u128(0x0d)];
        }),
        // DescriptorSet
        ("descriptor_set -> None", |s| s.descriptor_set = None),
        ("descriptor.invoice_line_template", |s| {
            if let Some(set) = s.descriptor_set.as_mut() {
                set.invoice_line_template = Some("{plan}".to_owned());
            }
        }),
        ("descriptor.gl_code", |s| {
            if let Some(set) = s.descriptor_set.as_mut() {
                set.gl_code = Some("4001".to_owned());
            }
        }),
        ("descriptor.itemization_rule", |s| {
            if let Some(set) = s.descriptor_set.as_mut() {
                set.itemization_rule = Some("rolled_up".to_owned());
            }
        }),
        ("descriptor.additional value", |s| {
            if let Some(set) = s.descriptor_set.as_mut() {
                set.additional
                    .insert("segment".to_owned(), "ent".to_owned());
            }
        }),
        ("descriptor.additional key", |s| {
            if let Some(set) = s.descriptor_set.as_mut() {
                set.additional.remove("segment");
                set.additional.insert("tier".to_owned(), "smb".to_owned());
            }
        }),
    ]
}

/// One mutator per field of `KeyWindows` and `WindowInterval`.
///
/// A fourth table for the same reason the other three are separate: the entry
/// count, not a boundary in the encoder.
fn window_mutators() -> Vec<Mutator> {
    vec![
        ("windows: one group dropped", |s| {
            s.windows = vec![maximal_window_group(phase_id(0x11), "EU")];
        }),
        ("windows -> empty", |s| s.windows = Vec::new()),
        ("group.scope_key", |s| {
            s.windows[0] = maximal_window_group(phase_id(0x11), "US");
        }),
        // The usage line, on the **one** path where a scope key is pinned with no
        // row beside it. In `put_price_record` these two axes are also columns of
        // `PriceRow`, so the digest moves with them whether or not the key frames
        // them; a window group carries no row, so here nothing stands in. The two
        // entries differ from each other only in the dimension, and the runner
        // requires every mutant to pin distinctly — which is what makes the tenth
        // axis asserted rather than covered by the ninth.
        ("group.scope_key.meter", |s| {
            s.windows[0].scope_key = line_key(Some("cloudlets"), "");
        }),
        ("group.scope_key.dimension_key", |s| {
            s.windows[0].scope_key = line_key(Some("cloudlets"), "eu-west");
        }),
        ("group: one interval dropped", |s| {
            s.windows[0].intervals.pop();
        }),
        ("interval.effective_from", |s| {
            s.windows[0].intervals[0].effective_from = at(5);
        }),
        ("interval.effective_to", |s| {
            s.windows[0].intervals[0].effective_to = Some(at(7));
        }),
        // The open end is a *state* of the interval and not an absent value, so
        // it has to pin differently from every closed end — the ABSENT marker's
        // whole job, asserted here as it is for `BandTop::open`.
        ("interval.effective_to -> None", |s| {
            s.windows[0].intervals[0].effective_to = None;
        }),
        ("interval.state", |s| {
            s.windows[0].intervals[0].state = WindowState::Cancelled;
        }),
    ]
}

fn row_mutators() -> Vec<Mutator> {
    vec![
        // PriceRecord
        ("rows: one dropped", |s| {
            s.rows = vec![maximal_record(0xb001)];
        }),
        ("record.price_id", |s| {
            s.rows[0].price_id = Uuid::from_u128(0xb003);
        }),
        ("record.tax_inclusive", |s| s.rows[0].tax_inclusive = false),
        // D-110's category, which a `PATCH` can move on a draft. Without this
        // entry a reviewer could approve a plan billing one tax category and the
        // commit publish another with every digest equal — the same
        // re-verification hole `sku_id` records.
        ("record.tax_category_ref", |s| {
            s.rows[0].tax_category_ref = Some("reduced".to_owned());
        }),
        // The absence half, spelled the way `interval.effective_to -> None` is:
        // a row that states no category and one that states `standard` are
        // different content, and the pair above and here is what makes the
        // encoding of this field observable at all rather than only its presence.
        ("record.tax_category_ref -> None", |s| {
            s.rows[0].tax_category_ref = None;
        }),
        ("record.billing_timing", |s| {
            s.rows[0].billing_timing = Some("arrears".to_owned());
        }),
        // Slice 6's proration contract, one entry per member: the whole point
        // of framing it is that a reviewer who approved one anchor cannot have a
        // commit publish another with the digest equal, and a table that moved
        // the value wholesale would pass while three of the four members went
        // unframed.
        ("record.proration_contract (present vs absent)", |s| {
            s.rows[0].proration_contract = Some(contract(
                BillingAnchorPolicy::CalendarMonth,
                ProrationBasis::CalendarDaysActual,
                false,
            ));
        }),
        ("record.proration_contract.billing_anchor_policy", |s| {
            s.rows[0].proration_contract = Some(contract(
                BillingAnchorPolicy::SubscriptionStart,
                ProrationBasis::CalendarDaysActual,
                false,
            ));
        }),
        ("record.proration_contract.anchor_day", |s| {
            s.rows[0].proration_contract = Some(contract(
                BillingAnchorPolicy::FixedDay(AnchorDay::new(28).expect("a day of the month")),
                ProrationBasis::CalendarDaysActual,
                false,
            ));
        }),
        ("record.proration_contract.proration_basis", |s| {
            s.rows[0].proration_contract = Some(contract(
                BillingAnchorPolicy::CalendarMonth,
                ProrationBasis::BySecond,
                false,
            ));
        }),
        ("record.proration_contract.credit_on_downgrade", |s| {
            s.rows[0].proration_contract = Some(contract(
                BillingAnchorPolicy::CalendarMonth,
                ProrationBasis::CalendarDaysActual,
                true,
            ));
        }),
        ("record.rounding_policy_ref", |s| {
            s.rows[0].rounding_policy_ref = Some("half-even".to_owned());
        }),
        ("record.grandfather_until", |s| {
            s.rows[0].grandfather_until = Some(at(21));
        }),
        ("record.supersedes_price_id", |s| {
            s.rows[0].supersedes_price_id = Some(Uuid::from_u128(0xfeed));
        }),
        ("record.lifecycle_state", |s| {
            s.rows[0].lifecycle_state = LifecycleState::Published;
        }),
        ("record.created_by", |s| {
            s.rows[0].created_by = Uuid::from_u128(0xac_11);
        }),
        ("record.created_at_utc", |s| {
            s.rows[0].created_at_utc = at(10);
        }),
        ("record.row_version", |s| {
            s.rows[0].row_version = RowVersion::new(5);
        }),
        // ScopeKey
        ("key.currency", |s| {
            s.rows[0].scope_key = key(ChargeKind::Usage, "EUR", "EU", phase_id(0x11));
        }),
        ("key.region", |s| {
            s.rows[0].scope_key = key(ChargeKind::Usage, "USD", "US", phase_id(0x11));
        }),
        ("key.phase", |s| {
            s.rows[0].scope_key = key(ChargeKind::Usage, "USD", "EU", phase_id(0x12));
        }),
        ("key.charge_kind", |s| {
            s.rows[0].scope_key = key(ChargeKind::Recurring, "USD", "EU", phase_id(0x11));
        }),
        ("key.price_eligibility", |s| {
            s.rows[0].scope_key = ScopeKey::new(
                plan(),
                CurrencyCode::new("USD").expect("three letters"),
                Region::new("EU").expect("a non-blank region"),
                phase_id(0x11),
                PriceEligibility::NewSubscriptionsOnly,
                ChargeKind::Usage,
                Cohort::None,
            )
            .expect("new_subscriptions_only pairs with cohort none");
        }),
        ("key.cohort", |s| {
            s.rows[0].scope_key = ScopeKey::new(
                plan(),
                CurrencyCode::new("USD").expect("three letters"),
                Region::new("EU").expect("a non-blank region"),
                phase_id(0x11),
                PriceEligibility::ExistingGrandfathered,
                ChargeKind::Usage,
                Cohort::Generation(at(6)),
            )
            .expect("existing_grandfathered pairs with a generation");
        }),
        // PriceRow
        ("row.charge_kind", |s| {
            s.rows[0].row.charge_kind = ChargeKind::Recurring;
        }),
        ("row.model_kind", |s| {
            s.rows[0].row.model_kind = Some(ModelKind::Volume);
        }),
        ("row.amount_minor", |s| {
            s.rows[0].row.amount_minor = Some(money(501));
        }),
        // D-311's `per_unit` rate (`v12`). It is the price of a metered row, and
        // for the length of one commit it was the only price column outside this
        // table: `amount_minor` had been pinned since the first version, the rate
        // was split out of it, and the split did not carry the pin. A reviewer who
        // approved `0.023` per GB and a commit that published `0.230` would have
        // matched on every digest, on a column `amount_minor` no longer holds.
        //
        // Both directions, as the reservation rate has: setting a rate where the
        // baseline has none is the same hole as moving one.
        ("row.unit_rate", |s| {
            s.rows[0].row.unit_rate = Some(rate(7));
        }),
        ("row.bands: one dropped", |s| {
            s.rows[0].row.bands = vec![TierBand::open(100, rate(5))];
        }),
        ("band.from_qty", |s| {
            s.rows[0].row.bands[0] = TierBand::closed(1, 100, rate(10));
        }),
        ("band.to_qty", |s| {
            s.rows[0].row.bands[0] = TierBand::closed(0, 99, rate(10));
        }),
        ("band.to_qty -> open", |s| {
            s.rows[0].row.bands[0] = TierBand {
                from_qty: 0,
                to_qty: BandTop::Open,
                unit_price_rate: rate(10),
            };
        }),
        ("band.unit_price_rate", |s| {
            s.rows[0].row.bands[0] = TierBand::closed(0, 100, rate(11));
        }),
        ("row.package_size", |s| {
            s.rows[0].row.package_size = Some(51);
        }),
        ("row.package_price_minor", |s| {
            s.rows[0].row.package_price_minor = Some(money(401));
        }),
        ("row.quantity_source", |s| {
            s.rows[0].row.quantity_source = Some(QuantitySource::SubscriptionSeatCount);
        }),
        ("row.manual_quantity", |s| {
            s.rows[0].row.manual_quantity = Some(1_000);
        }),
        ("row.meter", |s| {
            s.rows[0].row.meter = Some("api.bytes".to_owned());
        }),
        ("row.dimension_key", |s| {
            s.rows[0].row.dimension_key = "region:us".to_owned();
        }),
        ("row.billing_granularity", |s| {
            s.rows[0].row.billing_granularity = Some(BillingGranularity::PerDay);
        }),
        ("row.tier_aggregation_window", |s| {
            s.rows[0].row.tier_aggregation_window = Some(TierAggregationWindow::InvoicePeriod);
        }),
        ("row.tier_qualification_window", |s| {
            s.rows[0].row.tier_qualification_window = Some(TierQualificationWindow::TrailingPeriod);
        }),
        ("row.aggregation_function", |s| {
            s.rows[0].row.aggregation_function = Some(AggregationFunction::Sum);
        }),
        ("row.aggregation_granularity", |s| {
            s.rows[0].row.aggregation_granularity = Some(AggregationGranularity::Day);
        }),
        ("row.max_hold_granules", |s| {
            s.rows[0].row.max_hold_granules = Some(4);
        }),
        ("row.included_allowance -> None", |s| {
            s.rows[0].row.included_allowance = None;
        }),
        ("allowance.quantity", |s| {
            s.rows[0].row.included_allowance = Some(IncludedAllowance {
                quantity: 2_000,
                rollover_policy: RolloverPolicy::Carry,
            });
        }),
        ("allowance.rollover_policy", |s| {
            s.rows[0].row.included_allowance = Some(IncludedAllowance {
                quantity: 1_000,
                rollover_policy: RolloverPolicy::None,
            });
        }),
    ]
}

/// The Slice-10 mutators, split out because `row_mutators` outgrew the 200-line
/// cap when they arrived — the same split `plan_level_mutators` /
/// `child_mutators` / `window_mutators` already make. One list per family keeps
/// the reason a field is pinned next to the field.
fn slice10_row_mutators() -> Vec<Mutator> {
    vec![
        // The Slice-10 reservation pair (`v9`). Both halves get a mutator: the
        // rate because it is the money a reviewer approves, and the flavor
        // because flipping it moves the charge (`inst-rv-tier-q` vs
        // `inst-rv-level`) without moving the rate -- which is exactly the
        // mutation a rate-only pin would miss.
        ("row.reserved_rate", |s| {
            s.rows[0].row.reserved_rate =
                Some(RateMinor::from_minor_units(251).expect("a non-negative rate"));
        }),
        ("row.reserved_rate -> None", |s| {
            s.rows[0].row.reserved_rate = None;
        }),
        ("row.reservation_flavor", |s| {
            s.rows[0].row.reservation_flavor = Some(ReservationFlavor::Consumption);
        }),
        ("row.reservation_flavor -> None", |s| {
            s.rows[0].row.reservation_flavor = None;
        }),
        // The typed floors and the discount hook (`v10`). Each moves a distinct
        // consequence: who may buy (`min_qty_purchase`), what is billable
        // (`min_qty_usage`), what happens below the floor
        // (`min_qty_usage_fallback`), and which instrument discounts the line
        // (`discount_ref`).
        ("row.min_qty_purchase", |s| {
            s.rows[0].row.min_qty_purchase = Some(11);
        }),
        ("row.min_qty_purchase -> None", |s| {
            s.rows[0].row.min_qty_purchase = None;
        }),
        ("row.min_qty_usage", |s| {
            s.rows[0].row.min_qty_usage = Some(21);
        }),
        ("row.min_qty_usage_fallback -> None", |s| {
            s.rows[0].row.min_qty_usage_fallback = None;
        }),
        ("row.discount_ref", |s| {
            s.rows[0].row.discount_ref = Some("promo/autumn".to_owned());
        }),
        ("row.discount_ref -> None", |s| {
            s.rows[0].row.discount_ref = None;
        }),
    ]
}

/// Slice 6's **plan-level** contract mutators, split out of [`row_mutators`].
///
/// A second table rather than four more entries in the first, because
/// `clippy::too_many_lines` caps that one at 200 and it was at the edge — and
/// because these four move a field of the [`PlanShape`] itself rather than of a
/// row it carries, which is the same seam the two tables already sit either side
/// of.
fn plan_contract_mutators() -> Vec<Mutator> {
    vec![
        // Slice 6's plan-change contract, member by member. The edge list's
        // `None` and its empty vector are separate entries on purpose: the pin
        // frames them differently because `inst-pc-failsafe` gives them
        // different meanings, and a preimage that collapsed them would let a
        // plan leave self-service change without moving its digest.
        (
            "shape.change_contract.allowed_change_targets (absent)",
            |s| {
                s.change_contract.allowed_change_targets = None;
            },
        ),
        (
            "shape.change_contract.allowed_change_targets (empty)",
            |s| {
                s.change_contract.allowed_change_targets = Some(Vec::new());
            },
        ),
        (
            "shape.change_contract.allowed_change_targets (a different edge)",
            |s| {
                s.change_contract.allowed_change_targets = Some(vec![Uuid::from_u128(0xed_9e)]);
            },
        ),
        // Slice 10's composites (D-256). Four mutators, one per framed member,
        // because the census is what asserts the framing exists at all -- without
        // them D-256's `put_composite` block could be deleted and this suite
        // would stay green (found by review, D-259).
        ("shape.composites: added", |s| {
            s.composites
                .push(crate::domain::plan_shape::CompositeMeter {
                    composite_id: uuid::Uuid::from_u128(0xc0_11),
                    output_unit: "vm-hour".to_owned(),
                    constituent_units: vec!["vcpu".to_owned(), "ram".to_owned()],
                    formula: serde_json::json!({ "op": "weighted_sum", "weights": [1, 1] }),
                });
        }),
        ("shape.composites[0].output_unit", |s| {
            s.composites[0].output_unit = "pod-hour".to_owned();
        }),
        ("shape.composites[0].constituent_units", |s| {
            s.composites[0].constituent_units.push("disk".to_owned());
        }),
        // The one the v11 bump exists for: the weights move and nothing else.
        ("shape.composites[0].formula", |s| {
            s.composites[0].formula =
                serde_json::json!({ "op": "weighted_sum", "weights": [1, 4] });
        }),
        ("shape.entitlement_grants.plan_tier_ref", |s| {
            s.entitlement_grants.plan_tier_ref = Some("tier-platinum".to_owned());
        }),
        // **The flag's boolean, not its presence.** `base()` grants `sso: false`,
        // so this row is `put_grant_set`'s `put_bool(*on)` and nothing else --
        // the framed count and the framed key are both unchanged. It inserted
        // into an empty map until 2026-08-20, which the count alone explained.
        (
            "shape.entitlement_grants.plan_level.feature_flags (value)",
            |s| {
                s.entitlement_grants
                    .plan_level
                    .feature_flags
                    .insert("sso".to_owned(), true);
            },
        ),
        (
            "shape.entitlement_grants.plan_level.feature_flags (added)",
            |s| {
                s.entitlement_grants
                    .plan_level
                    .feature_flags
                    .insert("byok".to_owned(), true);
            },
        ),
        // The quota's integer, on the same reasoning, and it is the member whose
        // silent loss `put_entitlement_grants`' own doc names: a trial capped at
        // 20 cloudlets published as 20 000.
        ("shape.entitlement_grants.plan_level.quotas (value)", |s| {
            s.entitlement_grants
                .plan_level
                .quotas
                .insert("cloudlets".to_owned(), 20_000);
        }),
        ("shape.entitlement_grants.plan_level.quotas (added)", |s| {
            s.entitlement_grants
                .plan_level
                .quotas
                .insert("storage-gb".to_owned(), 1);
        }),
        // The per-phase set is framed through the same `put_grant_set`, so its
        // value half needs its own row: a phase-scoped quota is what D-41's
        // trial cap actually rides on.
        ("shape.entitlement_grants.per_phase (value)", |s| {
            s.entitlement_grants.per_phase.insert(
                phase_id(0x11).get(),
                grant_set(&[("beta", true)], &[("cloudlets", 3)]),
            );
        }),
        ("shape.entitlement_grants.per_phase (added)", |s| {
            s.entitlement_grants
                .per_phase
                .insert(uuid::Uuid::from_u128(0xf1), GrantSet::default());
        }),
        ("shape.change_contract.comparability_rank", |s| {
            s.change_contract.comparability_rank = Some(42);
        }),
        ("shape.change_contract.comparability_rank (absent)", |s| {
            s.change_contract.comparability_rank = None;
        }),
        ("shape.change_contract.usage_counter_on_plan_change", |s| {
            s.change_contract.usage_counter_on_plan_change = UsageCounterOnPlanChange::Carry;
        }),
    ]
}

// ---------------------------------------------------------------------------
// The pin moves for content
// ---------------------------------------------------------------------------

/// Each mutator moves the pin, and no two of them move it to the same place.
///
/// The second half is the one a framing bug fails: an unframed encoding does not
/// leave a digest *unchanged*, it makes two different shapes agree — which is
/// the pin verifying against content the reviewer never saw.
#[test]
fn every_hashed_field_moves_the_pin() {
    let base = base();
    let pinned = content_hash(&base);

    let mut seen: BTreeMap<[u8; 32], &'static str> = BTreeMap::new();
    seen.insert(pinned, "the unmutated shape");

    for (name, mutate) in mutators() {
        let mut shape = base.clone();
        mutate(&mut shape);
        assert_ne!(
            shape, base,
            "the mutator `{name}` did not change the shape at all"
        );
        let moved = content_hash(&shape);
        if let Some(other) = seen.insert(moved, name) {
            panic!("`{name}` and `{other}` pin identically");
        }
    }
}

/// A field that is absent and a field that is empty are different content.
///
/// This is what the NULL-safe marker buys, and it is the half a naive
/// `unwrap_or_default()` encoder silently loses.
#[test]
fn an_absent_field_and_an_empty_one_pin_differently() {
    let mut absent = base();
    absent.plan_tier = None;
    let mut empty = base();
    empty.plan_tier = Some(String::new());
    assert_ne!(content_hash(&absent), content_hash(&empty));
}

/// A character moved across a field boundary changes the pin.
///
/// Without the length prefixes `("ab", "c")` and `("a", "bc")` concatenate to
/// the same bytes, and a plan could be re-authored across two fields with its
/// approval still verifying.
#[test]
fn a_character_cannot_be_moved_across_a_field_boundary() {
    let mut left = base();
    left.plan_tier = Some("ab".to_owned());
    left.invoice_grouping_key = Some("c".to_owned());

    let mut right = base();
    right.plan_tier = Some("a".to_owned());
    right.invoice_grouping_key = Some("bc".to_owned());

    assert_ne!(content_hash(&left), content_hash(&right));
}

/// Two adjacent collections cannot be re-split between them.
///
/// The element count is framed ahead of the elements for exactly this: without
/// it, a plan with two add-on rules and one phase could hash as one with one
/// rule and two phases if the element encodings happened to line up.
#[test]
fn two_adjacent_collections_cannot_be_re_split() {
    let mut two_phases = base();
    two_phases.phases = PhaseGraph::new(vec![
        maximal_phase(0x11, 0, Some(phase_id(0x12))),
        maximal_phase(0x12, 1, None),
    ]);
    two_phases.addon_rules = vec![maximal_rule(0x21)];

    let mut one_phase = base();
    one_phase.phases = PhaseGraph::new(vec![maximal_phase(0x11, 0, Some(phase_id(0x12)))]);
    one_phase.addon_rules = vec![maximal_rule(0x21), maximal_rule(0x22)];

    assert_ne!(content_hash(&two_phases), content_hash(&one_phase));
}

// ---------------------------------------------------------------------------
// The pin does NOT move for the two exclusions, nor for query order
// ---------------------------------------------------------------------------

/// **The test that makes re-verification possible at all.**
///
/// `evaluated_at` is the instant the pipeline is run at, and the approve path
/// re-assembles the shape at a different one by construction. If it were hashed,
/// every approve would answer `APPROVAL_CONTENT_MISMATCH` — for every plan, with
/// nothing having changed — and the failure would look exactly like the attack
/// the pin exists to catch.
#[test]
fn the_pin_ignores_the_instant_the_pipeline_ran_at() {
    let submitted = base();
    let mut reviewed = base();
    reviewed.evaluated_at = at(18);
    assert_ne!(
        submitted.evaluated_at, reviewed.evaluated_at,
        "this test is about two different instants"
    );
    assert_eq!(content_hash(&submitted), content_hash(&reviewed));
}

/// The published past is context, not this revision's content.
///
/// It is re-derived on each assembly from rows this revision does not own and it
/// is not what a reviewer is shown. It also cannot move while a unit pends: a
/// `submitted` unit holds the plan's scope key through
/// `PENDING_CHANGE_UNIT_EXISTS`.
#[test]
fn the_pin_ignores_the_published_baseline() {
    let submitted = base();
    let mut reviewed = base();
    reviewed.baseline = None;
    assert_ne!(
        submitted.baseline, reviewed.baseline,
        "this test is about two different baselines"
    );
    assert_eq!(content_hash(&submitted), content_hash(&reviewed));
}

/// The row set is a set. A repository `ORDER BY` is not an authored fact.
#[test]
fn the_row_sets_query_order_is_not_content() {
    let straight = base();
    let mut reversed = base();
    reversed.rows.reverse();
    assert_ne!(straight.rows, reversed.rows, "the orders really differ");
    assert_eq!(content_hash(&straight), content_hash(&reversed));
}

/// The same, for every other collection whose order a read decides.
#[test]
fn the_other_collections_are_sets_too() {
    let straight = base();

    let mut phases = base();
    let mut reordered: Vec<PlanPhase> = phases.phases.phases().to_vec();
    reordered.reverse();
    phases.phases = PhaseGraph::new(reordered);
    assert_eq!(content_hash(&straight), content_hash(&phases));

    let mut rules = base();
    rules.addon_rules.reverse();
    assert_eq!(content_hash(&straight), content_hash(&rules));

    let mut edges = base();
    edges.addon_rules[0].depends_on.reverse();
    assert_eq!(content_hash(&straight), content_hash(&edges));

    let mut bands = base();
    bands.rows[0].row.bands.reverse();
    assert_eq!(content_hash(&straight), content_hash(&bands));

    let mut groups = base();
    groups.windows.reverse();
    assert_eq!(content_hash(&straight), content_hash(&groups));

    let mut intervals = base();
    intervals.windows[0].intervals.reverse();
    assert_eq!(content_hash(&straight), content_hash(&intervals));
}

/// Two shapes built independently, field for field, pin identically.
///
/// The stability the whole mechanism needs: the pin is a function of the value
/// and not of how the value was assembled or of any per-process state.
#[test]
fn two_independently_built_equal_shapes_pin_identically() {
    assert_eq!(content_hash(&base()), content_hash(&base()));
}

/// The same shape with its one window in each of the four states of §4's machine.
///
/// Not `base()` mutated in place, so that the interval under test is the group's
/// only member and nothing else in the group can be what an assertion below is
/// about.
fn one_window_in_state(state: WindowState) -> PlanShape {
    let mut shape = base();
    shape.windows = vec![KeyWindows {
        scope_key: key(ChargeKind::Usage, "USD", "EU", phase_id(0x11)),
        intervals: vec![WindowInterval::new(at(4), Some(at(8)), state)],
    }];
    shape
}

/// **`scheduled → active → expired` does not move the pin; `→ cancelled` does.**
///
/// The inverse of the `("interval.state", …)` mutator above, which moves the state
/// to `Cancelled` and therefore asserts only the half that must move. This is the
/// half that must **not**, and it is the one whose absence let the defect land:
/// `put_window_interval` framed `WindowState::as_str` verbatim, so an ordinary
/// `WindowActivationJob` tick re-keyed the digest of every pending unit.
///
/// Derived from the design set rather than from the encoder. §4 transitions 1 and
/// 2 (`inst-ws-activate`, `inst-ws-expire`) fire on `now` reaching a **stored
/// bound**, and D-99's paired clarification is explicit that they are *not publish
/// units* and re-project nothing, *"so the time-driven transitions change nothing
/// projected"* — a pin that moved on one would be a projected consequence of a
/// tick. §4 transition 3 (`inst-ws-cancel`) is the operator's act, a publish unit
/// under D-99 and always-material under D-62, so a reviewer is entitled to see it
/// and the digest has to carry it. Both directions here, because either one alone
/// is satisfiable by an encoder that is wrong in the other.
///
/// Byte-identity and not "verifies": `content_hash` returns the 32 bytes, so
/// `assert_eq!` over the arrays is the strongest available statement.
#[test]
fn the_clock_may_flip_a_window_but_not_the_pin() {
    let scheduled = one_window_in_state(WindowState::Scheduled);
    let active = one_window_in_state(WindowState::Active);
    let expired = one_window_in_state(WindowState::Expired);
    let cancelled = one_window_in_state(WindowState::Cancelled);

    // Without this the equalities below would be equalities about one shape.
    assert_ne!(scheduled, active, "these really are different shapes");
    assert_ne!(active, expired, "these really are different shapes");
    assert_ne!(scheduled, cancelled, "these really are different shapes");

    assert_eq!(
        content_hash(&scheduled),
        content_hash(&active),
        "an activation is the clock, not an author: `inst-ws-activate` may not \
         re-key a pending approval"
    );
    assert_eq!(
        content_hash(&scheduled),
        content_hash(&expired),
        "and neither may `inst-ws-expire`, which is the same boundary at the other \
         end of the interval"
    );
    assert_ne!(
        content_hash(&scheduled),
        content_hash(&cancelled),
        "a cancellation is a publish unit and always-material: the reviewer signs \
         for a key that still has its successor"
    );
}

// ---------------------------------------------------------------------------
// The frozen encoding
// ---------------------------------------------------------------------------

/// The byte-reproduction vector.
///
/// A digest over a fixed shape, written down. Every other test here is a
/// *relative* assertion — this moved, that did not — and a relative suite stays
/// green through a wholesale re-encoding that invalidates every pin in every
/// tenant. This is the one that does not.
///
/// Changing it is a **migration**, not an edit: a pin taken under the old
/// encoding no longer verifies, so every open approval unit refuses. The
/// domain-separation tag carries the version for exactly that reason.
///
/// Each entry below records the hole its move closed rather than re-pasting the
/// vector:
///
/// - `v1` → `v2`, when `PlanShape::windows` joined the preimage.
/// - `v2` → `v3`, when the window **state** stopped being framed verbatim. Four
///   tokens became two — `scheduled`/`active`/`expired` all frame as `live`,
///   `cancelled` keeps its own — because framing the clock's three let the
///   `WindowActivationJob`'s tick void a pending two-person approval with
///   `APPROVAL_CONTENT_MISMATCH` over content nobody had touched, and
///   `inst-ws-future-start` puts that boundary ahead of *every* pending unit. The
///   digest below is a **narrower** function of the shape than the one above it
///   was, which is the point: it no longer reads a fact the clock owns.
///   [`the_clock_may_flip_a_window_but_not_the_pin`] is the property; this is its
///   byte vector.
/// - `v3` → `v4`, on **2026-08-06**, when D-196's `meter` and `dimensionKey`
///   joined `put_scope_key`. The first two moves narrowed or extended what the
///   preimage reads; this one closed a hole in it. The axes were framed by
///   `put_price_row` and so a *record*'s digest already moved with them, but a
///   `KeyWindows` group carries no row — two window plans on two meters of one
///   market pinned identically, and an approve could be answered by a
///   re-derivation over the other line's coverage. `group.scope_key.meter` and
///   `group.scope_key.dimension_key` in the mutator table are that property; this
///   is its byte vector.
///
/// - `v4` → `v5`, on **2026-08-07**, when Slice 4's `tax_category_ref` joined
///   `put_price_record`. Like `v4` this closes a hole rather than moving a
///   boundary: the column landed with `pricing_price`, it is authored draft
///   content a `PATCH` moves under the row's tag, and D-48 makes it one of the
///   descriptor set's pinned v1 five riding the row — so a reviewer who approved
///   a plan billing `standard` and a commit that publishes `reduced` would have
///   matched on every digest. `record.tax_category_ref` in the mutator table is
///   that property; this is its byte vector.
///
/// - `v8` -> `v9`, on **2026-08-08**, when Slice 10's `reserved_rate` and
///   `reservation_flavor` joined `put_price_row`. A hole again rather than a
///   boundary, and the most direct money one this table offers: the pair is
///   authored draft content a `PATCH` moves, so a reviewer who approved a
///   reserved rate of 250 and a commit that publishes 100 matched on every
///   digest. The flavor travels with it because flipping `capacity` to
///   `consumption` moves the charge (`inst-rv-tier-q` against `inst-rv-level`)
///   **without moving the rate**, which is the mutation a rate-only pin would
///   miss. `row.reserved_rate` and `row.reservation_flavor` in the mutator
///   table are those properties; this is their byte vector.
///
/// - `v9` -> `v10`, on **2026-08-08**, in the same group: `min_qty_purchase`,
///   `min_qty_usage`, `min_qty_usage_fallback` and `discount_ref` joined
///   `put_price_row`. Four holes of four different kinds -- who may buy, what is
///   billable, what happens below the floor, and which instrument discounts the
///   line -- and all four authored content a `PATCH` moves. The four
///   `row.min_qty_*` / `row.discount_ref` mutators are those properties; this is
///   their byte vector.
///
/// - `v11` -> `v12`, on **2026-08-11**, when D-311 gave the `per_unit` rate a
///   column of its own and `row.unit_rate` joined `put_price_row`. `v11` itself
///   is absent from this table because it moved the **plan** shape and left this
///   digest where it stood. This one is the plainest hole of all: the price a
///   metered row is sold at moved out of `amount_minor`, so between the two
///   spellings a reviewer who approved `0.023` per GB and a commit that published
///   `0.230` matched on every digest while the pin still covered the now-empty
///   `amount_minor`. The band rates need no member of their own — they were
///   always covered, and only the scale of the integer changed, which is why this
///   bump is one field and not two. `row.unit_rate` in the mutator table is that
///   property; this is its byte vector.
///
/// - `v12` -> `v13`, on **2026-08-15**, when D-318 gave the plan a name and
///   `plan_name` joined `put_plan_shape`. Two things moved this vector at once
///   and both were meant: the domain separator, which prefixes every preimage,
///   and the new framed field, which `base()` now carries a value for so that
///   the golden bytes cover a non-empty name rather than a `None` marker. The
///   hole is `v4`'s in a non-money field — a reviewer approves the document that
///   says what the catalog will call this plan, and unframed the name could be
///   swapped between submit and approve with every digest equal. The three
///   `plan_name` rows in the mutator table are that property.
///
/// - `v13` -> `v14`, on **2026-08-15**, when D-319's plan-level period floor/cap
///   joined `put_plan_shape`. Like `v11` it moves the **plan** shape, so it is in
///   this table for the counter's sake rather than for a mutator of its own: the
///   bound is a child set of the revision, not a field of a price row. The hole it
///   closes is the sharpest of the plan-shape ones — a reviewer who approved a plan
///   carrying no minimum and a commit that publishes one at $500 a period matched on
///   every digest, and unlike a rate change there is no line on the invoice that
///   would have shown it. **Two entries one day apart, and neither absorbs the
///   other**: `v13` and this were authored concurrently against a common `v12`, so
///   the vector below is the first to cover both the name and the bound, and it
///   equals neither wave's own measurement taken alone.
///
/// What makes any of them an edit rather than a migration today is on
/// [`CONTENT_PIN_DOMAIN_SEP`](super::CONTENT_PIN_DOMAIN_SEP): this gear is not
/// deployed, so no durable row holds a `v1` or a `v2` digest. That argument expires
/// with the first deployment.
///
/// # The 2026-08-20 move is **not** in that table, and the distinction is the
/// whole discipline
///
/// The vector moved on 2026-08-20 with **no** generation bump, because nothing
/// about the encoding changed: `base()` grew a value where it had been holding a
/// `Default`. Three collections were empty in the fixture and are not any more —
/// `period_floor_caps`, `entitlement_grants` and `change_contract` — so the golden
/// bytes now cover a non-empty period bound, a grant set with a `false` among its
/// flags and a real quota, and an authored change-target edge list, rather than
/// three framed counts of zero. It is `v13`'s *"`base()` now carries a value for
/// it so that the golden bytes cover a non-empty name rather than a `None`
/// marker"* applied to the members that were still absent.
///
/// The reason it matters that this is a **fixture** change is that it looks
/// exactly like the other kind from a failing run: a diff that moves this
/// constant is either "the preimage function changed, invalidate every pending
/// unit" or "the fixture got wider, cover more of the preimage", and only one of
/// them is an edit. `CONTENT_PIN_DOMAIN_SEP` stayed at `v15` through that
/// widening, and the proof it was the second kind is that
/// `the_overlay_encoding_is_frozen` and `the_threshold_encoding_is_frozen` — over
/// fixtures that did not move — were unchanged.
///
/// **2026-09-09 (D-357, D-358) was the first kind, and the constant moved to
/// `v16`:** `put_plan_phase` now frames `PlanPhase::display_name`, and
/// `PhaseKind::as_str` renders `interim` where it rendered `intro`, so every
/// pending unit's digest is stale by construction. `maximal_phase` widened in the
/// same edit (`display_name: Some("Onboarding")`) so the vector covers the label's
/// value and not only its presence — the `tax_category_ref` argument below — and
/// the three `phase.display_name` mutators hold the transitions apart. The overlay
/// and threshold vectors are again unchanged, which is what separates the two
/// kinds when both happen at once.
///
/// `maximal_record`'s `tax_category_ref` carries an authored token (`standard`)
/// rather than an ABSENT marker, which is what puts the field's *value* into the
/// golden bytes. With it `None` the field's only mutator is a `None -> Some`, which
/// an encoder framing presence alone satisfies while leaving the value a reviewer
/// read out of the preimage — the hole D-110's own mutator comment says it closes.
/// The pair `record.tax_category_ref` / `record.tax_category_ref -> None` is what
/// closes it, and this is their byte vector.
#[test]
fn the_encoding_is_frozen() {
    assert_eq!(
        hex32(&content_hash(&base())),
        "25f4aab2a71e21213582e588091483efc5148eab9576d4e514d6103cd8904236"
    );
}

// ---------------------------------------------------------------------------
// The threshold-policy preimage — the second pinned subject (G6, D-10).
// ---------------------------------------------------------------------------

/// One version, built from the parts the pin frames.
fn threshold_version(
    version: u64,
    effective_from: OffsetDateTime,
    entries: Vec<(&str, ThresholdBasis)>,
) -> ThresholdVersion {
    ThresholdVersion::new(
        version,
        effective_from,
        entries
            .into_iter()
            .map(|(code, basis)| ThresholdEntry {
                currency: CurrencyCode::new(code).expect("a valid code"),
                basis,
            })
            .collect(),
    )
    .expect("a well-formed version")
}

/// The version every case below moves exactly one thing away from.
fn threshold_base() -> ThresholdVersion {
    threshold_version(
        3,
        at(9),
        vec![
            ("EUR", ThresholdBasis::Absolute { minor: 500 }),
            ("USD", ThresholdBasis::Percent { bp: 250 }),
        ],
    )
}

/// **Every field of the version moves the digest, one field at a time.**
///
/// The list-shaped assertion the plan preimage has, for the subject that arrived
/// with it. One case per field rather than one case moving everything, because a
/// single "something changed" assertion passes against an encoder that frames only
/// the first field it is given.
#[test]
fn every_field_of_a_threshold_version_moves_the_pin() {
    let base = threshold_content_hash(&threshold_base());

    // The version number. A pin that omitted it would verify a reviewer's signature
    // over one row set against another row set of the same shape — which is exactly
    // what `effective_version` resolves the policy through.
    assert_ne!(
        base,
        threshold_content_hash(&threshold_version(
            4,
            at(9),
            vec![
                ("EUR", ThresholdBasis::Absolute { minor: 500 }),
                ("USD", ThresholdBasis::Percent { bp: 250 }),
            ],
        ))
    );
    // The authored instant. It decides when approved thresholds start applying, so
    // a proposer who could move it after the signature moves the policy's start.
    assert_ne!(
        base,
        threshold_content_hash(&threshold_version(
            3,
            at(10),
            vec![
                ("EUR", ThresholdBasis::Absolute { minor: 500 }),
                ("USD", ThresholdBasis::Percent { bp: 250 }),
            ],
        ))
    );
    // A currency.
    assert_ne!(
        base,
        threshold_content_hash(&threshold_version(
            3,
            at(9),
            vec![
                ("GBP", ThresholdBasis::Absolute { minor: 500 }),
                ("USD", ThresholdBasis::Percent { bp: 250 }),
            ],
        ))
    );
    // A threshold value.
    assert_ne!(
        base,
        threshold_content_hash(&threshold_version(
            3,
            at(9),
            vec![
                ("EUR", ThresholdBasis::Absolute { minor: 501 }),
                ("USD", ThresholdBasis::Percent { bp: 250 }),
            ],
        ))
    );
    // An entry dropped — the widening hazard read backwards. `effective_version`
    // depends on this one: an `INSERT` of a currency an approved version did not
    // have is the single mutation the store's primary key still permits, and it is
    // caught only because adding or dropping an entry moves the digest.
    assert_ne!(
        base,
        threshold_content_hash(&threshold_version(
            3,
            at(9),
            vec![("EUR", ThresholdBasis::Absolute { minor: 500 })],
        ))
    );
}

/// The two bases are **tagged**, so equal numbers under different bases differ.
///
/// The case a two-nullable-column framing would have got wrong: `1000` minor units
/// and `1000` basis points are the same integer, and an encoder that framed the
/// value without its basis would pin a 10% threshold and a ten-unit threshold to
/// one digest.
#[test]
fn an_absolute_and_a_percent_threshold_of_the_same_number_pin_differently() {
    assert_ne!(
        threshold_content_hash(&threshold_version(
            0,
            at(9),
            vec![("EUR", ThresholdBasis::Absolute { minor: 1000 })],
        )),
        threshold_content_hash(&threshold_version(
            0,
            at(9),
            vec![("EUR", ThresholdBasis::Percent { bp: 1000 })],
        ))
    );
}

/// The two preimages live in **disjoint domains**, and each names its own
/// generation.
///
/// Two assertions and they are different properties. The first is the golden
/// vector one file over, restated as an inequality against a policy digest: a
/// threshold version and a plan shape share the `content_hash` column, so a shared
/// domain separator would leave `find_approved_for_content` matching across kinds
/// on a collision the `subject_ref` alone would have to catch. The second pins the
/// two counters *independently* — which is the property, rather than either value.
///
/// **The two counters move independently, and the literals below are the roster.**
/// The plan preimage is re-frozen whenever a field joins it, so its tag advances;
/// the threshold tag does not move with it, because a `ThresholdVersion` carries
/// neither a scope key nor a price row. That independence is what this case exists
/// for, and it is the reason neither generation is spelled in this paragraph.
#[test]
fn the_two_pin_domains_are_disjoint_and_each_names_its_own_generation() {
    assert_ne!(
        threshold_content_hash(&threshold_base()).as_slice(),
        content_hash(&base()).as_slice()
    );
    assert_eq!(
        super::CONTENT_PIN_DOMAIN_SEP,
        b"VHP-BSS-PRICING-APPROVAL-PIN-v16\x1f"
    );
    assert_eq!(
        super::THRESHOLD_PIN_DOMAIN_SEP,
        b"VHP-BSS-PRICING-THRESHOLD-PIN-v1\x1f"
    );
}

/// The threshold encoding is frozen too, and this is its recorded golden vector.
///
/// Same status as `the_encoding_is_frozen`: **not a value to update quietly.** A
/// diff that moves it is a re-freeze of the policy preimage, which invalidates
/// every pending D-10 unit in every tenant, and the remedy is a
/// `THRESHOLD_PIN_DOMAIN_SEP` bump with the drain the other constant's doc
/// describes — not a new constant pasted in from a failing run.
#[test]
fn the_threshold_encoding_is_frozen() {
    assert_eq!(
        hex32(&threshold_content_hash(&threshold_base())),
        "3f9d99001744c4974c955d3c882f686bf7d971250d4468b89fe82a54e400799f"
    );
}

/// **A tombstone's pin says "no thresholds", distinguishably from every particular
/// set** — D-185's requirement on this encoder.
///
/// The reviewer of a retirement signs a digest, and the whole safety of the decision
/// rests on that digest naming *this* content: an approver who signed "no thresholds"
/// must not have signed something a proposer could re-derive as an entry set, and the
/// converse. Three things have to hold, and none of them follows from the others:
///
/// * the tombstone's digest differs from an entry version of the same number and
///   instant — the entry count precedes the entries, so `0` and `1` frame differently;
/// * it still moves with `version` and with `effective_from`, because a retirement is
///   a version like any other and both fields are inside the pin;
/// * two tombstones agreeing on both fields pin **identically**, which is what makes
///   the re-derivation at approve time verifiable at all.
///
/// **No re-freeze, and that is measured rather than assumed.** No token was added to
/// the preimage: `ThresholdVersion::new` refuses an empty entry set, so a framed count
/// of zero is a preimage no non-tombstone version can produce, and the entry versions'
/// bytes are untouched — `the_threshold_encoding_is_frozen` above is unmoved, so
/// `THRESHOLD_PIN_DOMAIN_SEP` stays at `v1` and no pending unit is invalidated.
#[test]
fn a_tombstone_pins_distinguishably_from_every_entry_set() {
    let retirement = ThresholdVersion::tombstone(3, at(9));
    assert!(
        retirement.is_tombstone(),
        "the fixture has to be the tombstone or this case proves nothing"
    );

    // Against the version that shares its number and its instant and has entries.
    assert_ne!(
        hex32(&threshold_content_hash(&retirement)),
        hex32(&threshold_content_hash(&threshold_base())),
        "an approver signing 'no thresholds' must not be signing a digest an entry set can \
         re-derive to"
    );
    // And against the smallest entry set there is, which is the near-collision the
    // framed count is what prevents.
    assert_ne!(
        hex32(&threshold_content_hash(&retirement)),
        hex32(&threshold_content_hash(&threshold_version(
            3,
            at(9),
            vec![("EUR", ThresholdBasis::Absolute { minor: 0 })],
        ))),
        "a threshold of zero is a real threshold - everything in that currency is material - and \
         is not the absence of one"
    );

    // Both other fields still move it: a retirement is a version, so its number and
    // its start are as much the reviewer's business as an entry set's are.
    assert_ne!(
        hex32(&threshold_content_hash(&retirement)),
        hex32(&threshold_content_hash(&ThresholdVersion::tombstone(
            4,
            at(9)
        ))),
    );
    assert_ne!(
        hex32(&threshold_content_hash(&retirement)),
        hex32(&threshold_content_hash(&ThresholdVersion::tombstone(
            3,
            at(10)
        ))),
        "the instant the two-person rule comes back is inside the pin, so a proposer cannot move \
         it after the reviewer signed"
    );

    // And the re-derivation verifies: same version, same instant, same digest.
    assert_eq!(
        hex32(&threshold_content_hash(&retirement)),
        hex32(&threshold_content_hash(&ThresholdVersion::tombstone(
            3,
            at(9)
        ))),
        "a tombstone read back out of the store must digest to what was pinned, or every approve \
         of a retirement answers APPROVAL_CONTENT_MISMATCH"
    );
}

// ---------------------------------------------------------------------------
// The overlay pin (D-225)
// ---------------------------------------------------------------------------

/// A maximal overlay revision: every `Option` `Some`, every collection non-empty,
/// so a mutator below changes a value rather than filling in an absence.
fn overlay_base() -> OverlayRevision {
    OverlayRevision {
        price_overlay_id: Uuid::from_u128(0x0_a1),
        revision: 3,
        lifecycle_state: OverlayLifecycle::Draft,
        scope: ScopeSelector::scoped(
            ScopeClass::Brand,
            ScopeValue::new("acme").expect("a non-blank value"),
        )
        .expect("brand is not the global class"),
        precedence: 10,
        interval: OverlayInterval {
            from: Some(at(9)),
            to: Some(at(10)),
        },
        tax_basis: TaxBasis::DelegatedTariffs,
        disclosure: Disclosure::Restricted,
        target_ref: TargetRef {
            plans: vec![
                PlanId::new(Uuid::from_u128(1)),
                PlanId::new(Uuid::from_u128(2)),
            ],
        },
        lines: vec![
            OverlayLine {
                line_id: Uuid::from_u128(0x0_c1),
                key: LineKey::for_sku(
                    PlanId::new(Uuid::from_u128(1)),
                    TargetSku::new("sku-a").expect("a non-blank sku"),
                ),
                adjustment: Adjustment::Discount(Magnitude::PercentBp(1000)),
            },
            OverlayLine {
                line_id: Uuid::from_u128(0x0_c2),
                key: LineKey::list_default(),
                adjustment: Adjustment::Fixed(AmountSet::new([
                    (CurrencyCode::new("EUR").expect("a valid code"), 1200),
                    (CurrencyCode::new("USD").expect("a valid code"), 1500),
                ])),
            },
        ],
    }
}

/// Each mutator moves the overlay pin, and no two land on one digest.
///
/// `every_hashed_field_moves_the_pin`'s property, over the subject D-225's approval
/// unit pins. The second half is the one that matters: an unframed encoding does not
/// leave a digest unchanged, it makes two different overlays agree — and here that
/// would be a reviewer approving a discount they never saw.
#[test]
fn every_field_of_an_overlay_revision_moves_the_pin() {
    type Mutator = (&'static str, fn(&mut OverlayRevision));
    let mutators: Vec<Mutator> = vec![
        ("price_overlay_id", |o| {
            o.price_overlay_id = Uuid::from_u128(0x0_a2);
        }),
        ("revision", |o| o.revision = 4),
        ("lifecycle_state", |o| {
            o.lifecycle_state = OverlayLifecycle::Published;
        }),
        ("scope.class", |o| {
            o.scope = ScopeSelector::scoped(
                ScopeClass::Region,
                ScopeValue::new("acme").expect("a non-blank value"),
            )
            .expect("region is not the global class");
        }),
        ("scope.value", |o| {
            o.scope = ScopeSelector::scoped(
                ScopeClass::Brand,
                ScopeValue::new("other").expect("a non-blank value"),
            )
            .expect("brand is not the global class");
        }),
        ("precedence", |o| o.precedence = 11),
        ("interval.from", |o| o.interval.from = Some(at(8))),
        ("interval.to", |o| o.interval.to = Some(at(11))),
        ("interval.to = None", |o| o.interval.to = None),
        ("interval.from = None", |o| o.interval.from = None),
        ("tax_basis", |o| o.tax_basis = TaxBasis::Inclusive),
        ("disclosure", |o| o.disclosure = Disclosure::Public),
        ("target_ref.plans", |o| {
            o.target_ref.plans.push(PlanId::new(Uuid::from_u128(3)));
        }),
        ("lines.line_id", |o| {
            o.lines[0].line_id = Uuid::from_u128(0x0_c9);
        }),
        ("lines.key.plan_id", |o| {
            o.lines[0].key = LineKey::for_sku(
                PlanId::new(Uuid::from_u128(2)),
                TargetSku::new("sku-a").expect("a non-blank sku"),
            );
        }),
        ("lines.key.target_sku", |o| {
            o.lines[0].key = LineKey::for_sku(
                PlanId::new(Uuid::from_u128(1)),
                TargetSku::new("sku-b").expect("a non-blank sku"),
            );
        }),
        ("lines.key.cohort", |o| {
            o.lines[0].key = LineKey::for_plan(PlanId::new(Uuid::from_u128(1)))
                .for_cohort(at(9))
                .expect("a plan-keyed line may carry a cohort");
        }),
        ("lines.adjustment.kind", |o| {
            o.lines[0].adjustment = Adjustment::Markup(Magnitude::PercentBp(1000));
        }),
        ("lines.adjustment.magnitude", |o| {
            o.lines[0].adjustment = Adjustment::Discount(Magnitude::PercentBp(1001));
        }),
        ("lines.adjustment.amount currency", |o| {
            o.lines[1].adjustment = Adjustment::Fixed(AmountSet::new([
                (CurrencyCode::new("GBP").expect("a valid code"), 1200),
                (CurrencyCode::new("USD").expect("a valid code"), 1500),
            ]));
        }),
        ("lines.adjustment.amount minor", |o| {
            o.lines[1].adjustment = Adjustment::Fixed(AmountSet::new([
                (CurrencyCode::new("EUR").expect("a valid code"), 1201),
                (CurrencyCode::new("USD").expect("a valid code"), 1500),
            ]));
        }),
        ("a line removed", |o| {
            o.lines.pop();
        }),
    ];

    let base = overlay_base();
    let pinned = overlay_content_hash(&base);
    let mut seen: BTreeMap<[u8; 32], &'static str> = BTreeMap::new();
    seen.insert(pinned, "the unmutated overlay");

    for (name, mutate) in mutators {
        let mut overlay = base.clone();
        mutate(&mut overlay);
        assert_ne!(
            overlay, base,
            "the mutator `{name}` did not change the overlay at all"
        );
        if let Some(other) = seen.insert(overlay_content_hash(&overlay), name) {
            panic!("`{name}` and `{other}` pin identically");
        }
    }
}

/// **The line set is a set, and its query order is not content.**
///
/// `read_lines` orders by line id today; a repository that changed the `ORDER BY`
/// would otherwise invalidate every pending overlay unit in every tenant without
/// touching a single overlay.
#[test]
fn the_line_sets_query_order_is_not_content() {
    let forward = overlay_base();
    let mut reversed = overlay_base();
    reversed.lines.reverse();
    assert_ne!(forward.lines, reversed.lines, "the fixture must differ");
    assert_eq!(
        overlay_content_hash(&forward),
        overlay_content_hash(&reversed)
    );
}

/// The overlay encoding is frozen, and this is its recorded golden vector.
///
/// Same status as [`the_encoding_is_frozen`] and
/// [`the_threshold_encoding_is_frozen`]: **not a value to update quietly.** Every
/// other overlay case here is *relative* - a mutator moves the pin, two orderings
/// agree - and a relative suite is satisfied by an encoder that reframes the whole
/// preimage, because both sides move together. Only a byte vector can see that. A
/// diff that moves this one is a re-freeze of the overlay preimage and invalidates
/// every pending D-225 unit in every tenant; the remedy is an
/// `OVERLAY_PIN_DOMAIN_SEP` bump with the drain the plan constant's doc describes,
/// not a new constant pasted in from a failing run.
#[test]
fn the_overlay_encoding_is_frozen() {
    assert_eq!(
        hex32(&overlay_content_hash(&overlay_base())),
        "25b3500c67d0df351c87064d0dbe29331b65659cfb61b03d1e809625c3289b9d"
    );
}

/// The three pin domains are disjoint, and each names its own generation.
#[test]
fn the_overlay_pin_domain_is_its_own() {
    assert_ne!(
        overlay_content_hash(&overlay_base()).as_slice(),
        content_hash(&base()).as_slice()
    );
    assert_ne!(
        overlay_content_hash(&overlay_base()).as_slice(),
        threshold_content_hash(&threshold_base()).as_slice()
    );
    assert_eq!(
        OVERLAY_PIN_DOMAIN_SEP,
        b"VHP-BSS-PRICING-OVERLAY-PIN-v1\x1f"
    );
}
