//! What the payload promises, asserted against literals rather than against
//! the struct.
//!
//! The wire keys are the whole contract here: a delta is written once, frozen,
//! and read by a consumer that has only the JSON. So a Rust field rename that
//! silently renamed a persisted key would be invisible to every test that
//! reached the payload through the field it renamed — which is the reason
//! `CatalogEvent::as_str` is pinned against literals one module over, and the
//! reason every key below is spelled out.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use serde_json::json;
use std::collections::BTreeMap;

use super::{
    CROSS_BOUNDARY_CHANGE_POLICY, OverlayIndexDelta, OverlayIndexEntry, OverlaySubjectDelta,
    PROJECTED_ROW_STATES, PROJECTED_WINDOW_STATES, PlanSubjectDelta, RowResolutionProjection,
};
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{EntitlementGrants, PlanChangeContract};
use crate::domain::evaluation_policy::EVALUATION_POLICY_GENERATION;
use crate::domain::instant::{format_rfc3339, utc_ymd_hms};
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount, RateMinor};
use crate::domain::overlay::{
    Adjustment, Disclosure, LineKey, Magnitude, OverlayInterval, OverlayLifecycle, OverlayLine,
    OverlayRevision, ScopeClass, ScopeSelector, ScopeValue, TargetRef, TaxBasis,
};
use crate::domain::plan_shape::{
    AddonRule, BillingCycle, CompositeMeter, CustomIntervalUnit, DescriptorSet, Frequency,
    PeriodFloorCap, PhaseKind, PlanPhase,
};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{
    MinQtyUsageFallback, ModelKind, PriceRow, ReservationFlavor, TierBand,
};
use crate::domain::read_model::OverlayIndexShard;
use crate::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::window::{KeyWindows, WindowInterval, WindowState};
use time::OffsetDateTime;

fn plan_id() -> PlanId {
    PlanId::new(uuid::Uuid::from_u128(0x9_1a4))
}

fn terminal_phase() -> PhaseId {
    PhaseId::new(uuid::Uuid::from_u128(0xfa_5e))
}

/// The one derived meter every shape-only delta carries.
///
/// A weighted sum over two constituents, because a single-constituent definition
/// is one `CompositeArity` refuses and a fixture no publish would ever produce is
/// a fixture that proves nothing about what a version freezes.
fn composite() -> CompositeMeter {
    CompositeMeter {
        composite_id: uuid::Uuid::from_u128(0xc0_11),
        output_unit: "vm".to_owned(),
        constituent_units: vec!["vcpu".to_owned(), "ram".to_owned()],
        formula: json!({ "op": "weighted_sum", "weights": { "vcpu": 2, "ram": 1 } }),
    }
}

/// One add-on composition rule with **no member left at its default**.
///
/// `required` is `true` and the three quantity bounds are distinct non-`None`
/// values, so a renderer that transposed `minQty` and `maxQty`, or emitted a
/// constant for `required`, cannot pass the assertion this feeds. The two edge
/// lists are non-empty and different from each other for the same reason.
fn addon_rule() -> AddonRule {
    AddonRule {
        addon_sku_id: uuid::Uuid::from_u128(0xad_d0),
        required: true,
        min_qty: Some(1),
        max_qty: Some(9),
        step_qty: Some(3),
        price_override_ref: Some(uuid::Uuid::from_u128(0xad_d1)),
        depends_on: vec![uuid::Uuid::from_u128(0xad_d2)],
        conflicts_with: vec![
            uuid::Uuid::from_u128(0xad_d3),
            uuid::Uuid::from_u128(0xad_d4),
        ],
    }
}

/// A shape-only delta: a plan revision with children and **no price rows**.
fn shape_only() -> PlanSubjectDelta {
    PlanSubjectDelta {
        plan_id: plan_id(),
        revision: 3,
        lifecycle_state: LifecycleState::Published,
        sku_id: Some(uuid::Uuid::from_u128(0x5_c1)),
        plan_tier: Some("gold".to_owned()),
        plan_tier_override: false,
        billing_cycle: Some(BillingCycle::Recurring),
        frequency: Some(Frequency::Monthly),
        available_from: Some(utc_ymd_hms(2026, 8, 1, 0, 0, 0)),
        available_to: None,
        purchase_min_qty: None,
        purchase_max_qty: None,
        invoice_grouping_key: Some("bundle-a".to_owned()),
        phases: vec![PlanPhase {
            phase_id: terminal_phase(),
            kind: PhaseKind::Evergreen,
            display_name: None,
            ordinal: 0,
            converts_to_phase_id: None,
            phase_duration_days: None,
            display_trial_days: None,
        }],
        // **Fully specified, every one of the eight members carrying a value that
        // is not its `Default`.** `addon_rule_value`'s four wire keys —
        // `addonSkuId`, `priceOverrideRef`, `dependsOn`, `conflictsWith` — occur
        // nowhere else in the repository, so an empty vector here left the whole
        // add-on composition contract with no assertion anywhere in the crate: a
        // `minQty`/`maxQty` swap or a dropped `required` flag was invisible to
        // every gate, on a payload that is INSERT-only for seven years.
        addon_rules: vec![addon_rule()],
        descriptor_set: Some(DescriptorSet {
            invoice_line_template: Some("{plan}".to_owned()),
            gl_code: Some("4000".to_owned()),
            itemization_rule: Some("per_charge".to_owned()),
            additional: std::collections::BTreeMap::new(),
        }),
        // Two markets, and the second carries a cap the first does not: a
        // fixture with one entry cannot tell a renderer that emits the first
        // bound from one that emits the whole set, and a fixture with no cap
        // anywhere cannot tell `capMinor` from a hard-coded null (D-319).
        period_floor_caps: vec![
            PeriodFloorCap {
                currency: CurrencyCode::new("USD").expect("three letters"),
                region: Region::new("us").expect("non-blank"),
                floor_minor: Some(MinorAmount::new(50_000).expect("non-negative")),
                cap_minor: None,
            },
            PeriodFloorCap {
                currency: CurrencyCode::new("EUR").expect("three letters"),
                region: Region::new("de").expect("non-blank"),
                floor_minor: None,
                cap_minor: Some(MinorAmount::new(900_000).expect("non-negative")),
            },
        ],
        composites: vec![composite()],
        entitlement_grants: EntitlementGrants::default(),
        change_contract: PlanChangeContract::default(),
        prices: Vec::new(),
        tax_projection: BTreeMap::new(),
        windows: Vec::new(),
    }
}

fn graduated_row() -> PriceRecord {
    let mut row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::Graduated));
    row.meter = Some("api_calls".to_owned());
    row.bands = vec![
        TierBand::closed(
            0,
            100,
            RateMinor::from_nano_minor(0).expect("a non-negative rate"),
        ),
        TierBand::open(
            100,
            RateMinor::from_nano_minor(5_000_000_000).expect("a non-negative rate"),
        ),
    ];
    PriceRecord {
        price_id: uuid::Uuid::from_u128(0xb_0001),
        scope_key: ScopeKey::new(
            plan_id(),
            CurrencyCode::new("EUR").expect("three letters"),
            Region::new("eu").expect("a non-blank region"),
            terminal_phase(),
            PriceEligibility::AllSubscriptions,
            ChargeKind::Usage,
            Cohort::None,
        )
        .expect("the class pairs with cohort none"),
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: None,
        proration_contract: None,
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Published,
        created_by: uuid::Uuid::from_u128(0xac_10),
        created_at_utc: utc_ymd_hms(2026, 8, 1, 10, 0, 0),
        row_version: RowVersion::new(0),
    }
}

#[test]
fn the_plan_level_wire_keys_are_what_a_consumer_reads() {
    let value = shape_only().to_value();

    assert_eq!(value.get("planId"), Some(&json!(plan_id().get())));
    assert_eq!(value.get("revision"), Some(&json!(3)));
    assert_eq!(value.get("lifecycleState"), Some(&json!("published")));
    assert_eq!(
        value.get("skuId"),
        Some(&json!(uuid::Uuid::from_u128(0x5_c1)))
    );
    assert_eq!(value.get("planTier"), Some(&json!("gold")));
    assert_eq!(value.get("planTierOverride"), Some(&json!(false)));
    assert_eq!(value.get("billingCycle"), Some(&json!("recurring")));
    assert_eq!(
        value.get("frequency"),
        Some(&json!({ "token": "monthly" })),
        "a fixed frequency carries its token and no interval"
    );
    assert_eq!(value.get("availableTo"), Some(&json!(null)));
    assert_eq!(value.get("invoiceGroupingKey"), Some(&json!("bundle-a")));
    assert_eq!(
        value.get("phases"),
        Some(&json!([{
            "phaseId": terminal_phase().get(),
            "kind": "evergreen",
            // D-357: present as `null` when the operator gave no label, so a
            // consumer reads one shape for every phase.
            "displayName": null,
            "ordinal": 0,
            "convertsToPhaseId": null,
            "phaseDurationDays": null,
            "displayTrialDays": null,
        }]))
    );

    // D-357 / D-358: a labelled interim phase renders its label as presentation
    // and its kind under the renamed token; the terminal phase's absence of a
    // label stays `null` rather than disappearing.
    let onboarding = PhaseId::new(uuid::Uuid::from_u128(0x0b_0a));
    let mut labelled = shape_only();
    labelled.phases = vec![
        PlanPhase {
            phase_id: onboarding,
            kind: PhaseKind::Interim,
            display_name: Some("Onboarding".to_owned()),
            ordinal: 0,
            converts_to_phase_id: Some(terminal_phase()),
            phase_duration_days: Some(30),
            display_trial_days: None,
        },
        PlanPhase {
            phase_id: terminal_phase(),
            kind: PhaseKind::Evergreen,
            display_name: None,
            ordinal: 1,
            converts_to_phase_id: None,
            phase_duration_days: None,
            display_trial_days: None,
        },
    ];
    let phases = labelled.to_value()["phases"].clone();
    assert_eq!(phases[0]["kind"], json!("interim"), "{phases}");
    assert_eq!(phases[0]["displayName"], json!("Onboarding"), "{phases}");
    assert_eq!(phases[1]["displayName"], json!(null), "{phases}");
    assert!(
        !phases.to_string().contains("intro"),
        "no phase renders the pre-D-358 token: {phases}"
    );
    assert_eq!(
        value.get("descriptorSet"),
        Some(&json!({
            "invoiceLineTemplate": "{plan}",
            "glCode": "4000",
            "itemizationRule": "per_charge",
            "additional": {},
        }))
    );
}

/// **`inst-cm-frozen`'s consumer-facing half.**
///
/// The instruction has two clauses: the catalog *freezes* the definition and
/// Rating *computes from the snapshot*. The first was true of the approval content
/// pin from v11 and of nothing a consumer reads — this module contained the word
/// "composite" zero times — so a formula could be hashed, displayed to a reviewer,
/// signed for, and never reach the artifact Rating resolves through
/// `pricingSnapshotRef`. This asserts the second clause.
///
/// The whole object rather than the presence of a key, because a renderer that
/// emitted `composites: []` on a populated set would satisfy any weaker assertion
/// while freezing nothing — and an empty list is the value a consumer cannot tell
/// from "this plan defines no composite".
///
/// The **formula is compared verbatim**, nested object and all. It is opaque to
/// this crate by A4, which means this gear has no vocabulary to normalize it into
/// and no right to: what a version freezes has to be what the author wrote and the
/// reviewer signed.
#[test]
fn the_composite_definitions_reach_the_payload_whole() {
    let value = shape_only().to_value();

    assert_eq!(
        value.get("composites"),
        Some(&json!([{
            "compositeId": uuid::Uuid::from_u128(0xc0_11),
            "outputUnit": "vm",
            "constituentUnits": ["vcpu", "ram"],
            "formula": { "op": "weighted_sum", "weights": { "vcpu": 2, "ram": 1 } },
        }])),
        "{value}"
    );
}

/// A revision defining no composite renders the member as an **empty list**, never
/// as an absent key.
///
/// Stated because the two are different answers to a consumer: a missing member is
/// a payload from a gear that does not carry composites at all, and the read model
/// is INSERT-only over the seven-year horizon — so a consumer meeting both
/// spellings across versions would have to guess which era it was reading.
#[test]
fn a_revision_with_no_composite_renders_an_empty_list_rather_than_no_member() {
    let delta = PlanSubjectDelta {
        composites: Vec::new(),
        ..shape_only()
    };

    assert_eq!(delta.to_value().get("composites"), Some(&json!([])));
}

#[test]
fn a_custom_frequency_carries_its_interval_and_not_only_its_token() {
    // The pairing `chk_pricing_plan_custom_interval_pairing` exists to keep: a
    // frozen `custom_every_n` with no `n` is a billing period nobody can
    // compute, and the token alone cannot reconstruct one.
    let delta = PlanSubjectDelta {
        frequency: Some(Frequency::CustomEveryN {
            n: 45,
            unit: CustomIntervalUnit::Days,
        }),
        ..shape_only()
    };

    assert_eq!(
        delta.to_value().get("frequency"),
        Some(&json!({ "token": "custom_every_n", "n": 45, "unit": "days" }))
    );
}

#[test]
fn the_lifecycle_field_renders_retired_as_well_as_published() {
    // The whole of D-128 in one assertion. The revision's state is a projected
    // plan-subject field because it is what sellability predicate (4) reads at
    // the pin, and a retired plan can never publish again — so if `retired` did
    // not reach the payload, nothing would ever correct the delta and the read
    // model would advertise a retired plan as sellable permanently.
    for state in [LifecycleState::Published, LifecycleState::Retired] {
        let delta = PlanSubjectDelta {
            lifecycle_state: state,
            ..shape_only()
        };
        assert_eq!(
            delta.to_value().get("lifecycleState"),
            Some(&json!(state.as_str())),
            "the projected lifecycle state must reach the payload"
        );
    }
}

#[test]
fn the_payload_carries_the_declared_generation_and_this_file_does_not_spell_it() {
    // Read from the constant, never from a literal here: a second spelling of
    // the generation is a test that stays green through the one edit it exists
    // to catch (D-162 — the document declares it, `evaluation_policy_tests`
    // pins the constant to the document, and this pins the payload to the
    // constant).
    assert_eq!(
        shape_only().to_value().get("evaluationPolicyVersion"),
        Some(&json!(EVALUATION_POLICY_GENERATION))
    );
}

// ---------------------------------------------------------------------------
// `inst-bt-usage` — the timing a non-recurring row does not author.
// ---------------------------------------------------------------------------

/// One row of `kind`, carrying whatever timing it was handed.
fn row_of_kind(kind: ChargeKind, authored: Option<&str>) -> PriceRecord {
    let mut record = graduated_row();
    record.row = PriceRow::new(kind, Some(ModelKind::Flat));
    record.row.amount_minor = Some(MinorAmount::new(1000).expect("a non-negative amount"));
    record.scope_key = ScopeKey::new(
        plan_id(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        terminal_phase(),
        PriceEligibility::AllSubscriptions,
        kind,
        Cohort::None,
    )
    .expect("the class pairs with cohort none");
    record.billing_timing = authored.map(ToOwned::to_owned);
    record
}

/// The projected `billingTiming` of the delta's single row.
fn projected_timing(record: PriceRecord) -> serde_json::Value {
    let delta = PlanSubjectDelta {
        prices: vec![record],
        ..shape_only()
    };
    delta.to_value()["prices"][0]["billingTiming"].clone()
}

/// A usage row is implicitly `arrears` and a one-time row implicitly `advance`
/// — projected constants, never authored, so Billing reads the same field on
/// every line of a hybrid instead of a null it has to interpret.
#[test]
fn a_non_recurring_row_projects_its_constant_timing() {
    assert_eq!(
        projected_timing(row_of_kind(ChargeKind::Usage, None)),
        json!("arrears")
    );
    assert_eq!(
        projected_timing(row_of_kind(ChargeKind::OneTime, None)),
        json!("advance")
    );
    assert_eq!(
        projected_timing(row_of_kind(ChargeKind::OneTimeSetup, None)),
        json!("advance")
    );
}

/// The constant is a **projection**, not a default: a value that reached the
/// column on a non-recurring row does not get to speak for it. Anything else
/// would make `inst-bt-usage` a defaultable field, and Billing's deferral would
/// depend on whether an author had typed into a column the design says is not
/// theirs.
#[test]
fn an_authored_value_on_a_non_recurring_row_does_not_displace_the_constant() {
    assert_eq!(
        projected_timing(row_of_kind(ChargeKind::Usage, Some("advance"))),
        json!("arrears")
    );
}

/// The recurring row is the only one that authors the field, and it projects
/// exactly what it authored — `inst-bt-required` guarantees there is one.
#[test]
fn a_recurring_row_projects_exactly_what_it_authored() {
    for timing in ["advance", "arrears"] {
        assert_eq!(
            projected_timing(row_of_kind(ChargeKind::Recurring, Some(timing))),
            json!(timing)
        );
    }
}

#[test]
fn every_plan_subject_carries_the_cross_boundary_marker_and_no_warning_text() {
    // D-169 clause (1). The marker is a launch-constant, tenant-wide value on
    // every resolved plan subject; the text that used to sit beside it is not a
    // catalog field at all - its normative home is PRD AC #66, on the surface
    // that renders the warning and takes the operator's confirmation.
    //
    // Read from the constant rather than from a literal, for
    // `evaluationPolicyVersion`'s reason one test up: a second spelling of the
    // value is a test that stays green through the one edit it exists to catch.
    let with_row = PlanSubjectDelta {
        prices: vec![graduated_row()],
        ..shape_only()
    };
    for delta in [shape_only(), with_row] {
        let value = delta.to_value();
        assert_eq!(
            value.get("crossBoundaryChangePolicy"),
            Some(&json!(CROSS_BOUNDARY_CHANGE_POLICY)),
            "{value}"
        );
        // Under any spelling: the field left the contract, so a payload
        // carrying it would be publishing a sentence nobody authored into an
        // INSERT-only store on the seven-year horizon.
        let rendered = value.to_string();
        assert!(
            !rendered.to_ascii_lowercase().contains("warningtext"),
            "no delta may carry a warning text: {rendered}"
        );
    }
}

#[test]
fn the_marker_is_the_value_the_design_set_names_verbatim() {
    // Asserted against the literal here and nowhere else in the crate, for
    // `CatalogEvent::as_str`'s reason: `06-consumer-contracts.md` sec 6 names
    // this string, a consumer matches on it exactly, and nothing else in the
    // crate would notice a typo.
    assert_eq!(CROSS_BOUNDARY_CHANGE_POLICY, "cancel_plus_new");
}

#[test]
fn a_revision_with_no_price_rows_still_renders_a_well_formed_payload() {
    // A shape-only revision — one that changes a descriptor or a phase duration
    // and authors no price rows — is an ordinary publish, so its delta is an
    // ordinary payload with an empty row set rather than an absent one. A
    // missing key and an empty array are not the same claim about a plan.
    // The add-on set is emptied here rather than read off the fixture, which now
    // carries one: the claim being made is about the **empty** spelling, and a
    // fixture that happens to be empty stops making it the day the fixture is
    // enriched.
    let value = PlanSubjectDelta {
        addon_rules: Vec::new(),
        ..shape_only()
    }
    .to_value();

    assert!(value.is_object());
    assert_eq!(value.get("prices"), Some(&json!([])));
    assert_eq!(value.get("addonRules"), Some(&json!([])));
}

/// **The whole add-on composition contract reaches the payload**, member by
/// member, against a literal.
///
/// `the_composite_definitions_reach_the_payload_whole`'s arrangement one member
/// over, and for the sharper reason: `addonSkuId`, `priceOverrideRef`,
/// `dependsOn` and `conflictsWith` are spelled nowhere else in the repository, so
/// until 2026-08-20 `addon_rule_value` could have been emptied, renamed key by
/// key, or had `minQty` and `maxQty` transposed, with the whole crate green.
///
/// The keys are literals rather than derived from the struct's field names — this
/// file's module doc gives the reason: a Rust field rename that silently renamed
/// a persisted key would otherwise be invisible.
#[test]
fn the_addon_rules_reach_the_payload_whole() {
    let value = shape_only().to_value();

    assert_eq!(
        value.get("addonRules"),
        Some(&json!([{
            "addonSkuId": uuid::Uuid::from_u128(0xad_d0),
            "required": true,
            "minQty": 1,
            "maxQty": 9,
            "stepQty": 3,
            "priceOverrideRef": uuid::Uuid::from_u128(0xad_d1),
            "dependsOn": [uuid::Uuid::from_u128(0xad_d2)],
            "conflictsWith": [
                uuid::Uuid::from_u128(0xad_d3),
                uuid::Uuid::from_u128(0xad_d4),
            ],
        }])),
        "{value}"
    );
}

/// **The period floor/cap set reaches the payload whole** (D-319).
///
/// The fixture was built with two markets and complementary bounds precisely so
/// that this comparison can be made — its own comment says a one-entry fixture
/// "cannot tell a renderer that emits the first bound from one that emits the
/// whole set, and a fixture with no cap anywhere cannot tell `capMinor` from a
/// hard-coded null". Nothing in this file read the member back, so the fixture
/// read as coverage of `period_floor_cap_value` and was none: the only assertion
/// on these wire keys anywhere was `tests/rest_migrated_origin_snapshots.rs`,
/// which asserts the **synthesis** renderer's separate copy.
#[test]
fn the_period_floor_caps_reach_the_payload_whole() {
    let value = shape_only().to_value();

    assert_eq!(
        value.get("periodFloorCaps"),
        Some(&json!([
            { "currency": "USD", "region": "us", "floorMinor": 50_000, "capMinor": null },
            { "currency": "EUR", "region": "de", "floorMinor": null, "capMinor": 900_000 },
        ])),
        "{value}"
    );
}

/// **A `per_unit` row's rate reaches the frozen payload, on a member of its own**
/// (D-311).
///
/// The read model is what a consumer prices against, and for the length of one
/// commit the `per_unit` price was written to a column no projection member
/// named: `amountMinor` went `null` on the kind that used to carry it and the
/// rate went nowhere. A consumer would have read a published metered row with no
/// price at all — which the shape-only assertions on this file could not see,
/// because a member that is absent from both the payload and the assertion looks
/// exactly like a member nobody wanted.
///
/// It is a **separate member** rather than a second meaning for `amountMinor`,
/// so a reader cannot take a nano-minor rate for a whole-minor amount by reading
/// a name that did not change.
#[test]
fn a_per_unit_rows_rate_reaches_the_payload_on_its_own_member() {
    let mut record = graduated_row();
    record.row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::PerUnit));
    record.row.meter = Some("api_calls".to_owned());
    // 0.023 USD per GB-month -- S3's rate, and the case D-311 opens with. It has
    // no representation in whole minor units at all.
    record.row.unit_rate =
        Some(RateMinor::from_nano_minor(2_300_000_000).expect("a non-negative rate"));

    let delta = PlanSubjectDelta {
        prices: vec![record],
        ..shape_only()
    };
    let value = delta.to_value();
    let rows = value
        .get("prices")
        .expect("prices")
        .as_array()
        .expect("array");
    let row = &rows[0];

    assert_eq!(
        row.get("unitRateNanoMinor"),
        Some(&json!(2_300_000_000_i64)),
        "the rate a metered row is sold at, in the scale it is stored in"
    );
    assert_eq!(
        row.get("amountMinor"),
        Some(&json!(null)),
        "and not in the amount column it left: two priced members are two \
         competing prices, which is what the split exists to prevent"
    );
}

#[test]
fn a_price_row_freezes_its_key_its_shape_and_its_bands() {
    let delta = PlanSubjectDelta {
        prices: vec![graduated_row()],
        ..shape_only()
    };
    let value = delta.to_value();
    let rows = value
        .get("prices")
        .expect("prices")
        .as_array()
        .expect("array");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];

    assert_eq!(
        row.get("priceId"),
        Some(&json!(uuid::Uuid::from_u128(0xb_0001)))
    );
    assert_eq!(row.get("lifecycleState"), Some(&json!("published")));
    assert_eq!(row.get("modelKind"), Some(&json!("graduated")));
    assert_eq!(row.get("chargeKind"), Some(&json!("usage")));
    assert_eq!(row.get("meter"), Some(&json!("api_calls")));
    assert_eq!(row.get("roundingPolicyRef"), Some(&json!("half_up")));
    assert_eq!(
        row.get("scopeKey"),
        Some(&json!({
            "planId": plan_id().get(),
            "currency": "EUR",
            "region": "eu",
            "priceOverlay": "base",
            "phase": terminal_phase().get(),
            "priceEligibility": "all_subscriptions",
            "chargeKind": "usage",
            "cohort": null,
            // Axes 9 and 10 (D-196). `null` on this row because the fixture's key
            // carries no line; the delta is where a consumer resolving a metered
            // plan reads which line a published usage row prices, and before
            // D-196 it could not need to — one market held one usage row.
            "meter": null,
            "dimensionKey": null,
        })),
        "all ten canonical axes, in the normative order"
    );
    assert_eq!(
        row.get("bands"),
        Some(&json!([
            { "fromQty": 0, "toQty": 100, "unitPriceNanoMinor": 0 },
            { "fromQty": 100, "toQty": null, "unitPriceNanoMinor": 5_000_000_000_i64 },
        ])),
        "an open top is null, never a sentinel a reader could compare against; \
         the band price is a rate in its own scale (D-311), and the member is \
         renamed rather than reused so a reader cannot take nano for minor"
    );
}

// ---------------------------------------------------------------------------
// D-45 / D-130 — the allowance compile is materialized here, and nowhere else.
// ---------------------------------------------------------------------------

fn rate(nano: i64) -> RateMinor {
    RateMinor::from_nano_minor(nano).expect("a non-negative rate")
}

/// A graduated usage row whose ladder is **priced from the origin**.
///
/// [`graduated_row`]'s own first band is `$0`, which is the hand-authored
/// allowance `ALLOWANCE_DOUBLE_FREE` refuses beside a declaration — so it is the
/// one fixture in this file an allowance cannot be hung on, and hanging one on it
/// would have produced a case that asserted the *absence* of a compile and read
/// as asserting its presence.
fn priced_from_the_origin() -> PriceRecord {
    let mut record = graduated_row();
    record.row.bands = vec![
        TierBand::closed(0, 1_000, rate(5_000_000_000)),
        TierBand::open(1_000, rate(3_000_000_000)),
    ];
    record
}

fn projected(record: PriceRecord) -> serde_json::Value {
    PlanSubjectDelta {
        prices: vec![record],
        ..shape_only()
    }
    .to_value()
    .get("prices")
    .expect("prices")
    .as_array()
    .expect("array")[0]
        .clone()
}

/// `inst-ac-band` + `inst-ac-marker` on a tiered row: the `$0` band is
/// prepended, every authored bound is offset by `+N`, and the marker rides beside
/// them.
#[test]
fn a_tiered_allowance_row_freezes_the_compiled_ladder_and_the_marker() {
    let mut record = priced_from_the_origin();
    record.row.included_allowance = Some(crate::domain::price_row::IncludedAllowance {
        quantity: 100,
        rollover_policy: crate::domain::price_row::RolloverPolicy::None,
    });

    let row = projected(record);

    assert_eq!(
        row.get("bands"),
        Some(&json!([
            { "fromQty": 0, "toQty": 100, "unitPriceNanoMinor": 0 },
            { "fromQty": 100, "toQty": 1_100, "unitPriceNanoMinor": 5_000_000_000_i64 },
            { "fromQty": 1_100, "toQty": null, "unitPriceNanoMinor": 3_000_000_000_i64 },
        ])),
        "the compiled set, not the authored one: this is what a consumer rates from"
    );
    assert_eq!(
        row.get("allowanceMarker"),
        Some(&json!({
            "quantity": 100,
            "rolloverPolicy": "none",
            "source": "compiled",
            "authoredModelKind": "graduated",
        })),
        "display and the included-vs-billed split read the marker, never a $0 band"
    );
    assert_eq!(
        row.get("includedAllowance"),
        Some(&json!({ "quantity": 100, "rolloverPolicy": "none" })),
        "and the authored declaration rides beside it: D-130 keeps the compile's own input"
    );
}

/// `inst-ac-band`'s untiered branch: the projection presents `graduated`, the
/// rate moves into the top band, and the truth row is untouched.
#[test]
fn an_untiered_allowance_row_projects_graduated_with_the_rate_folded_into_the_top_band() {
    let mut record = graduated_row();
    record.row = PriceRow::new(ChargeKind::Usage, Some(ModelKind::PerUnit));
    record.row.meter = Some("api_calls".to_owned());
    record.row.unit_rate = Some(rate(2_300_000_000));
    record.row.included_allowance = Some(crate::domain::price_row::IncludedAllowance {
        quantity: 50,
        rollover_policy: crate::domain::price_row::RolloverPolicy::None,
    });
    let truth = record.row.clone();

    let row = projected(record);

    assert_eq!(row.get("modelKind"), Some(&json!("graduated")));
    assert_eq!(
        row.get("bands"),
        Some(&json!([
            { "fromQty": 0, "toQty": 50, "unitPriceNanoMinor": 0 },
            { "fromQty": 50, "toQty": null, "unitPriceNanoMinor": 2_300_000_000_i64 },
        ]))
    );
    assert_eq!(
        row.get("unitRateNanoMinor"),
        Some(&json!(null)),
        "the rate is the top band's now, and a payload carrying both would hand a consumer two \
         competing prices"
    );
    assert_eq!(
        row.get("allowanceMarker")
            .and_then(|m| m.get("authoredModelKind")),
        Some(&json!("per_unit")),
        "the authored kind is retained beside the marker (D-59): it is what says the top \
         band's rate is a folded unitRateNanoMinor"
    );

    // D-130: the projection is a projection. The truth row still reads `per_unit`
    // with its rate intact and zero bands.
    assert_eq!(truth.model_kind, Some(ModelKind::PerUnit));
    assert_eq!(truth.unit_rate, Some(rate(2_300_000_000)));
    assert!(truth.bands.is_empty());
}

/// AC #90a — compile-equivalence, asserted at the wire.
///
/// Every priced member of the compiled row equals the hand-authored equivalent's.
/// If the two ever diverge, Rating bills an allowance row differently from the
/// row an operator would have written by hand to mean the same thing, and no
/// other test in this file would notice.
#[test]
fn the_projected_allowance_row_is_the_hand_authored_equivalent_member_for_member() {
    let mut declared = priced_from_the_origin();
    declared.row.included_allowance = Some(crate::domain::price_row::IncludedAllowance {
        quantity: 100,
        rollover_policy: crate::domain::price_row::RolloverPolicy::None,
    });

    let mut by_hand = priced_from_the_origin();
    by_hand.row.bands = vec![
        TierBand::closed(0, 100, rate(0)),
        TierBand::closed(100, 1_100, rate(5_000_000_000)),
        TierBand::open(1_100, rate(3_000_000_000)),
    ];

    let compiled = projected(declared);
    let authored = projected(by_hand);

    for member in ["modelKind", "bands", "unitRateNanoMinor", "amountMinor"] {
        assert_eq!(
            compiled.get(member),
            authored.get(member),
            "{member} diverges, so the two rows do not rate the same"
        );
    }
    // And the one member that must differ, or the marker would be inferable from
    // a `$0` band after all — which is the inference D-45 exists to replace.
    assert_eq!(authored.get("allowanceMarker"), Some(&json!(null)));
    assert!(
        compiled
            .get("allowanceMarker")
            .is_some_and(|m| !m.is_null())
    );
}

/// The negative half: a row declaring no allowance freezes an explicit `null`
/// marker and its own authored ladder.
#[test]
fn a_row_with_no_allowance_freezes_a_null_marker_and_its_authored_shape() {
    let row = projected(priced_from_the_origin());

    assert_eq!(
        row.get("allowanceMarker"),
        Some(&serde_json::Value::Null),
        "present and null, not absent: a consumer tells `this gear does not publish the field` \
         from `this row does not carry one` by the key"
    );
    assert_eq!(row.get("includedAllowance"), Some(&serde_json::Value::Null));
    assert_eq!(
        row.get("bands"),
        Some(&json!([
            { "fromQty": 0, "toQty": 1_000, "unitPriceNanoMinor": 5_000_000_000_i64 },
            { "fromQty": 1_000, "toQty": null, "unitPriceNanoMinor": 3_000_000_000_i64 },
        ])),
        "the authored ladder, unmoved: the compile fires on no row that declares nothing"
    );
}

/// A `carry` declaration compiles no band artifact, so the projection carries the
/// declaration and **no** marker.
///
/// Such a row cannot be authored or published today (`PRIMITIVE_RULES_UNBUILT`),
/// and this is here because the projector answers for rows whatever refused them:
/// a marker on a carry row would be a `$0` band's worth of free quantity that
/// `inst-ac-carry`'s grant is *also* supposed to give.
#[test]
fn a_carry_declaration_freezes_no_band_artifact() {
    let mut record = priced_from_the_origin();
    record.row.included_allowance = Some(crate::domain::price_row::IncludedAllowance {
        quantity: 100,
        rollover_policy: crate::domain::price_row::RolloverPolicy::Carry,
    });

    let row = projected(record);

    assert_eq!(row.get("allowanceMarker"), Some(&serde_json::Value::Null));
    assert_eq!(
        row.get("includedAllowance"),
        Some(&json!({ "quantity": 100, "rolloverPolicy": "carry" }))
    );
    assert_eq!(
        row.get("bands"),
        Some(&json!([
            { "fromQty": 0, "toQty": 1_000, "unitPriceNanoMinor": 5_000_000_000_i64 },
            { "fromQty": 1_000, "toQty": null, "unitPriceNanoMinor": 3_000_000_000_i64 },
        ]))
    );
}

/// Slice 10's six columns reach the frozen payload (`inst-rv-attrs`,
/// `inst-ft-typed`, `inst-ft-fallback`, `inst-dr-boundary`).
///
/// **This case exists because a probe found its absence.** Replacing
/// `reservedRateMinor` with a hard `null` in `row_value` reddened *nothing*
/// across the whole fast suite: the projection is where a value freezes into an
/// immutable version on the >= 7-year horizon and where Rating reads the
/// self-service reserved rate (`inst-rv-runtime`), so a field silently dropped
/// here is a charge Rating cannot compute from a snapshot that looks complete.
///
/// Every field is asserted rather than a representative one, for
/// `the_full_content_round_trips`'s reason: the six arrived in two commits and
/// the wire name is the consumer's contract, so a typo in one of them is a
/// consumer reading `null` forever.
#[test]
fn a_price_row_freezes_the_slice_ten_primitives() {
    let mut record = graduated_row();
    record.row.reserved_rate = Some(RateMinor::from_minor_units(250).expect("a non-negative rate"));
    record.row.reservation_flavor = Some(ReservationFlavor::Capacity);
    record.row.min_qty_purchase = Some(7);
    record.row.min_qty_usage = Some(11);
    record.row.min_qty_usage_fallback = Some(MinQtyUsageFallback::Exception);
    record.row.discount_ref = Some("promo/spring".to_owned());

    let delta = PlanSubjectDelta {
        prices: vec![record],
        ..shape_only()
    };
    let value = delta.to_value();
    let rows = value
        .get("prices")
        .expect("prices")
        .as_array()
        .expect("array");
    let row = &rows[0];

    // 250 minor units at the stored 10^-9 scale. The key and the scale moved
    // together (Z5-3): renaming the field without moving the expected value
    // would have left this test asserting a number the projection cannot emit.
    assert_eq!(
        row.get("reservedRateNanoMinor"),
        Some(&json!(250_000_000_000_i64))
    );
    assert_eq!(row.get("reservationFlavor"), Some(&json!("capacity")));
    assert_eq!(row.get("minQtyPurchase"), Some(&json!(7)));
    assert_eq!(row.get("minQtyUsage"), Some(&json!(11)));
    assert_eq!(row.get("minQtyUsageFallback"), Some(&json!("exception")));
    assert_eq!(row.get("discountRef"), Some(&json!("promo/spring")));
}

/// The negative half: a row authoring none of them freezes explicit `null`s
/// rather than omitting the keys.
///
/// A consumer distinguishes "this gear does not publish the field" from "this
/// row does not carry it" by the key's presence, and `inst-rv-runtime` has
/// Rating sourcing the reserved rate from here — so an absent key and a null
/// key are different answers and only one of them is true.
#[test]
fn a_row_carrying_no_slice_ten_primitive_freezes_nulls_rather_than_omitting_them() {
    let delta = PlanSubjectDelta {
        prices: vec![graduated_row()],
        ..shape_only()
    };
    let value = delta.to_value();
    let row = &value
        .get("prices")
        .expect("prices")
        .as_array()
        .expect("array")[0];

    for key in [
        "reservedRateNanoMinor",
        "reservationFlavor",
        "minQtyPurchase",
        "minQtyUsage",
        "minQtyUsageFallback",
        "discountRef",
    ] {
        assert_eq!(
            row.get(key),
            Some(&serde_json::Value::Null),
            "{key} must be present and null, not absent"
        );
    }
}

/// One fact, one spelling, inside one frozen document.
///
/// The row carries `dimensionKey` and so does its `scopeKey`, and on an
/// undimensioned line both say "there is no dimension here". A consumer joining
/// the two — which is what the member pair is for, since D-196 made the line the
/// thing that tells one published usage row of a market from another — must not
/// have to know that one member spells the absence as the empty string and the
/// other as `null`.
///
/// **The dimensioned row is the control and is not decoration.** An
/// absent-vs-absent comparison over a fixture that never sets the column
/// compares nothing with nothing: rendering the member as a constant `null` on
/// every row would erase the discriminator from the whole read model and satisfy
/// both assertions below. The first case is what fails then.
#[test]
fn a_rows_dimension_key_is_spelled_the_way_its_scope_key_spells_it() {
    let mut dimensioned = graduated_row();
    dimensioned.row.dimension_key = "region=eu".to_owned();
    dimensioned.scope_key = dimensioned
        .scope_key
        .clone()
        .with_usage_line(
            Some(Meter::new("api_calls").expect("a non-blank meter")),
            DimensionKey::new("region=eu"),
        )
        .expect("a usage key carries its line");

    let rendered = |record| {
        let delta = PlanSubjectDelta {
            prices: vec![record],
            ..shape_only()
        };
        delta.to_value()
    };

    let value = rendered(dimensioned);
    let row = &value
        .get("prices")
        .expect("prices")
        .as_array()
        .expect("array")[0];
    assert_eq!(
        row.get("dimensionKey"),
        Some(&serde_json::json!("region=eu")),
        "an authored line reaches the member a consumer resolves it by"
    );
    assert_eq!(
        row.get("dimensionKey"),
        row.get("scopeKey").expect("scopeKey").get("dimensionKey"),
        "the row and its key state one fact once"
    );

    let value = rendered(graduated_row());
    let row = &value
        .get("prices")
        .expect("prices")
        .as_array()
        .expect("array")[0];
    assert_eq!(
        row.get("dimensionKey"),
        Some(&serde_json::Value::Null),
        "an absent dimension is `null`, as it is on the scope key"
    );
    assert_eq!(
        row.get("dimensionKey"),
        row.get("scopeKey").expect("scopeKey").get("dimensionKey"),
        "and the absence, too, is stated once"
    );
}

#[test]
fn the_projected_row_states_are_the_two_that_are_not_never_published_drafts() {
    // D-121's set, and `superseded` is in it although nothing in this gear can
    // produce one: rating rates past instants, so a changeover's predecessor
    // must survive re-projection or yesterday's `t` fails closed on a covered
    // period. Listing it now means the day D-88 or D-100 lands, only the
    // horizon is owed.
    assert_eq!(
        PROJECTED_ROW_STATES,
        &[LifecycleState::Published, LifecycleState::Superseded]
    );
    assert!(
        !PROJECTED_ROW_STATES.contains(&LifecycleState::Draft),
        "a never-published draft is exactly what D-121 excludes"
    );
}

// ---------------------------------------------------------------------------
// The window facts (D-99, D-121)
// ---------------------------------------------------------------------------

/// Every axis of the scope key, and one window, on the plan's recurring key.
fn recurring_key() -> ScopeKey {
    ScopeKey::new(
        plan_id(),
        CurrencyCode::new("USD").expect("iso currency"),
        Region::new("EU").expect("region"),
        terminal_phase(),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("a valid canonical scope key")
}

fn at(day: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 9, day, 0, 0, 0)
}

/// D-121's projected window states: `scheduled | active | expired`.
///
/// A CANCELLED window is not projected - it is not history a consumer resolves
/// against, it is a schedule that never happened. An `expired` window IS
/// projected: rating pins current versions and rates past instants, so dropping
/// the predecessor's expired interval fails a legitimately covered arrears
/// period closed.
#[test]
fn the_projected_window_states_are_the_three_d121_names() {
    assert_eq!(
        PROJECTED_WINDOW_STATES,
        [
            WindowState::Scheduled,
            WindowState::Active,
            WindowState::Expired
        ]
    );
    assert!(
        !PROJECTED_WINDOW_STATES.contains(&WindowState::Cancelled),
        "a cancelled window is a schedule that never happened"
    );
    assert!(
        PROJECTED_WINDOW_STATES.contains(&WindowState::Expired),
        "the predecessor's expired interval is what an arrears period at a past \
         instant resolves against"
    );
    // Every state accounted for: three in, one out. A fifth state fails this
    // rather than silently joining or missing the projection.
    assert_eq!(WindowState::ALL.len(), 4);
}

/// The delta carries intervals and states and a derived coverage end - never a
/// point-in-time boolean (D-99).
///
/// The `activeNow`-shaped absence is what this test is for. Were the payload to
/// carry one, every activation and expiry would owe a re-projection of an
/// INSERT-only store whose whole contract is that a completed version never
/// changes - which is precisely why `inst-ws-publishunit` makes those two
/// transitions *not* publish units.
#[test]
fn the_payload_carries_intervals_and_a_coverage_end_and_no_active_flag() {
    let delta = PlanSubjectDelta {
        windows: vec![KeyWindows {
            scope_key: recurring_key(),
            intervals: vec![
                WindowInterval::new(at(10), Some(at(20)), WindowState::Expired),
                WindowInterval::new(at(20), Some(at(30)), WindowState::Active),
            ],
        }],
        ..shape_only()
    };

    let value = delta.to_value();
    let groups = value
        .get("windows")
        .expect("windows")
        .as_array()
        .expect("array");
    assert_eq!(groups.len(), 1, "one key, one group");
    let group = &groups[0];

    // The key the facts are filed under, rendered axis by axis exactly as a
    // price row's is - a consumer matches on axes, not on a display string.
    assert_eq!(
        group.get("scopeKey").and_then(|k| k.get("chargeKind")),
        Some(&json!("recurring"))
    );
    // The intervals, in time order, each with its state.
    assert_eq!(
        group.get("intervals"),
        Some(&json!([
            {
                "effectiveFrom": format_rfc3339(at(10)),
                "effectiveTo": format_rfc3339(at(20)),
                "state": "expired",
            },
            {
                "effectiveFrom": format_rfc3339(at(20)),
                "effectiveTo": format_rfc3339(at(30)),
                "state": "active",
            },
        ]))
    );
    // The derived end, as a discriminated object: a bare null would have to
    // stand for "covered forever" and "covered nowhere" at once, which are
    // opposite answers under the D-80 horizon predicate.
    assert_eq!(
        group.get("coverageEnd"),
        Some(&json!({ "kind": "ends", "at": format_rfc3339(at(30)) }))
    );

    // And no point-in-time answer anywhere in the document, under any spelling.
    let rendered = value.to_string().to_ascii_lowercase();
    for forbidden in ["activenow", "isactive", "activeat", "sellablenow"] {
        assert!(
            !rendered.contains(forbidden),
            "a frozen delta must not answer a question about the reader's clock \
             ({forbidden}): {rendered}"
        );
    }
}

/// An open-ended key renders the open end as its own kind, and a key with no
/// coverage renders as uncovered - the two answers a nullable instant would
/// have had to share.
#[test]
fn open_ended_and_uncovered_are_two_different_payload_answers() {
    let open = PlanSubjectDelta {
        windows: vec![KeyWindows {
            scope_key: recurring_key(),
            intervals: vec![WindowInterval::new(at(10), None, WindowState::Active)],
        }],
        ..shape_only()
    };
    let uncovered = PlanSubjectDelta {
        windows: vec![KeyWindows {
            scope_key: recurring_key(),
            intervals: Vec::new(),
        }],
        ..shape_only()
    };

    let end = |delta: PlanSubjectDelta| {
        delta
            .to_value()
            .get("windows")
            .expect("windows")
            .as_array()
            .expect("array")[0]
            .get("coverageEnd")
            .cloned()
            .expect("coverageEnd")
    };

    assert_eq!(end(open), json!({ "kind": "open_ended", "at": null }));
    assert_eq!(end(uncovered), json!({ "kind": "uncovered", "at": null }));
}

/// A plan with no windows renders an empty array rather than an absent key: a
/// missing key and an empty array are not the same claim about a plan.
#[test]
fn a_plan_with_no_windows_still_renders_the_key() {
    assert_eq!(shape_only().to_value().get("windows"), Some(&json!([])));
}

// ---------------------------------------------------------------------------
// The overlay plane's two subjects (D-112, D-133, D-234).
// ---------------------------------------------------------------------------

fn overlay_revision() -> OverlayRevision {
    OverlayRevision {
        price_overlay_id: uuid::Uuid::from_u128(0x0_1e_a1),
        revision: 2,
        lifecycle_state: OverlayLifecycle::Published,
        scope: ScopeSelector::scoped(
            ScopeClass::Partner,
            ScopeValue::new("acme").expect("a value"),
        )
        .expect("a valued class"),
        precedence: 40,
        interval: OverlayInterval {
            from: Some(utc_ymd_hms(2026, 9, 1, 0, 0, 0)),
            to: None,
        },
        tax_basis: TaxBasis::Exclusive,
        disclosure: Disclosure::Restricted,
        target_ref: TargetRef {
            plans: vec![PlanId::new(uuid::Uuid::from_u128(0x_91_a0))],
        },
        lines: vec![OverlayLine {
            line_id: uuid::Uuid::from_u128(0x_11_e1),
            key: LineKey::for_plan(PlanId::new(uuid::Uuid::from_u128(0x_91_a0))),
            adjustment: Adjustment::Discount(Magnitude::PercentBp(1500)),
        }],
    }
}

#[test]
fn an_overlay_subject_delta_renders_every_field_of_the_revision_it_froze() {
    // The wire keys are the contract, exactly as they are for the plan subject:
    // a delta is written once, frozen on the seven-year horizon, and read by a
    // consumer that has only the JSON.
    let value = OverlaySubjectDelta {
        content: overlay_revision(),
    }
    .to_value();

    assert_eq!(
        value
            .get("priceOverlayId")
            .and_then(serde_json::Value::as_str),
        Some(uuid::Uuid::from_u128(0x0_1e_a1).to_string().as_str())
    );
    assert_eq!(value.get("revision"), Some(&json!(2)));
    assert_eq!(value.get("lifecycleState"), Some(&json!("published")));
    assert_eq!(value.get("scopeClass"), Some(&json!("partner")));
    assert_eq!(value.get("scopeValue"), Some(&json!("acme")));
    assert_eq!(value.get("precedence"), Some(&json!(40)));
    assert_eq!(value.get("taxBasis"), Some(&json!("exclusive")));
    assert_eq!(value.get("disclosure"), Some(&json!("restricted")));
    assert_eq!(
        value.get("effectiveTo"),
        Some(&json!(null)),
        "open-ended renders the key with null, never omits it"
    );
    let lines = value
        .get("lines")
        .and_then(serde_json::Value::as_array)
        .expect("lines");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].get("kind"), Some(&json!("discount")));
}

#[test]
fn an_overlay_index_shard_orders_its_entries_by_precedence_then_id() {
    // The payload is frozen and INSERT-only, so its order is part of what one
    // version says. `inst-plv-class-tiebreak` names the total order
    // `precedence -> class order -> overlay id`; inside one shard the class is
    // constant by construction, so what remains is precedence then id.
    //
    // Rendered in an order derived from the store's row order instead, two
    // projections of one set would be two payloads.
    let low = uuid::Uuid::from_u128(0x_a1);
    let high = uuid::Uuid::from_u128(0x_b2);
    let delta = OverlayIndexDelta {
        shard: OverlayIndexShard::Global,
        entries: vec![
            OverlayIndexEntry {
                price_overlay_id: high,
                interval: OverlayInterval::default(),
                precedence: 10,
            },
            OverlayIndexEntry {
                price_overlay_id: low,
                interval: OverlayInterval::default(),
                precedence: 40,
            },
            OverlayIndexEntry {
                price_overlay_id: low,
                interval: OverlayInterval::default(),
                precedence: 10,
            },
        ],
    };

    let value = delta.to_value();
    assert_eq!(value.get("scopeClass"), Some(&json!("global")));
    assert_eq!(value.get("scopeValue"), Some(&json!("global")));
    let ids: Vec<&str> = value
        .get("overlays")
        .and_then(serde_json::Value::as_array)
        .expect("overlays")
        .iter()
        .map(|entry| {
            entry
                .get("priceOverlayId")
                .and_then(serde_json::Value::as_str)
                .expect("id")
        })
        .collect();
    assert_eq!(
        ids,
        vec![low.to_string(), high.to_string(), low.to_string()],
        "precedence 10 before 40, and inside precedence 10 the smaller id first"
    );
}

#[test]
fn an_empty_shard_renders_the_key_rather_than_omitting_it() {
    // The same claim the plan subject's empty window array makes: a shard with
    // no live overlay is a statement, and a missing key is not one. It is also
    // the reachable case - a revision moving its scope value rewrites the shard
    // it left, and that shard may now be empty.
    let value = OverlayIndexDelta {
        shard: OverlayIndexShard::Global,
        entries: Vec::new(),
    }
    .to_value();

    assert_eq!(value.get("overlays"), Some(&json!([])));
}

// ---------------------------------------------------------------------------
// D-154's resolved category and C3's GA gate — the derived pair (§6).
// ---------------------------------------------------------------------------

/// A delta carrying one row and the tax facts derived for it.
fn delta_with_tax(record: PriceRecord, tax: RowResolutionProjection) -> PlanSubjectDelta {
    let mut delta = shape_only();
    let price_id = record.price_id;
    delta.prices = vec![record];
    delta.tax_projection = [(price_id, tax)].into_iter().collect();
    delta
}

/// The payload carries **both** derived facts beside the authored column.
///
/// The authored `taxCategoryRef` and the resolved `resolvedTaxCategory` are two
/// fields, not one: D-110 makes the row the source of truth and D-154 freezes
/// what a consumer should use, and a payload rendering only the resolved value
/// would leave an operator unable to see what they actually authored.
#[test]
fn a_projected_row_carries_the_resolved_category_and_the_ga_flag() {
    let mut record = graduated_row();
    record.tax_category_ref = None;
    record.tax_inclusive = true;
    let delta = delta_with_tax(
        record,
        RowResolutionProjection {
            resolved_tax_category: Some("standard".to_owned()),
            resolved_rounding_policy: Some("half_up".to_owned()),
            not_sellable_ga: true,
        },
    );

    let value = delta.to_value();
    let row = &value["prices"][0];

    assert_eq!(
        row["taxCategoryRef"],
        json!(null),
        "the authored column is what the row states, and it states nothing"
    );
    assert_eq!(
        row["resolvedTaxCategory"], "standard",
        "and the resolved value is the region default D-154 froze"
    );
    assert_eq!(row["notSellableGa"], true);
}

/// A row with no derived facts renders the pair as absent rather than omitting
/// the keys.
///
/// A consumer must be able to tell *this version resolved nothing* from *this
/// field is not part of the contract*; an omitted key says the second.
#[test]
fn a_row_with_no_tax_projection_still_renders_both_keys() {
    let delta = {
        let mut d = shape_only();
        d.prices = vec![graduated_row()];
        d
    };

    let value = delta.to_value();
    let row = &value["prices"][0];

    assert_eq!(row["resolvedTaxCategory"], json!(null));
    assert_eq!(
        row["notSellableGa"], false,
        "absent is not gated: the gate is a positive fact a publish derives"
    );
}

/// The projection is **per row**, so one gated market does not gate its sibling.
///
/// `inst-td-gagate` is explicit that the flag is per `(currency, region)` market
/// and never per plan: "a plan selling tax-exclusive in US and tax-inclusive in
/// EU is gated **only** on its EU market(s)".
#[test]
fn the_ga_flag_is_carried_per_row_and_not_across_the_plan() {
    let mut gated = graduated_row();
    gated.tax_inclusive = true;
    let mut open = graduated_row();
    open.price_id = uuid::Uuid::from_u128(0xb0_02);
    open.tax_inclusive = false;

    let mut delta = shape_only();
    delta.tax_projection = [
        (
            gated.price_id,
            RowResolutionProjection {
                resolved_tax_category: Some("standard".to_owned()),
                resolved_rounding_policy: Some("half_up".to_owned()),
                not_sellable_ga: true,
            },
        ),
        (
            open.price_id,
            RowResolutionProjection {
                resolved_tax_category: Some("standard".to_owned()),
                resolved_rounding_policy: Some("half_up".to_owned()),
                not_sellable_ga: false,
            },
        ),
    ]
    .into_iter()
    .collect();
    delta.prices = vec![gated, open];

    let value = delta.to_value();

    assert_eq!(value["prices"][0]["notSellableGa"], true);
    assert_eq!(
        value["prices"][1]["notSellableGa"], false,
        "the sibling market stays sellable"
    );
}

// ---------------------------------------------------------------------------
// `inst-rc-ids` (Slice 6) — the three ids every downstream artifact resolves by.
// ---------------------------------------------------------------------------

/// `{skuId, planId, priceId}` are exposed on every projected artifact.
///
/// §3 step 1 says the ids are "stable … never re-used across revisions", and
/// that half is **structural**: `pricing_price` is append-only and a price id is
/// caller-supplied at creation, so no writer in this gear can re-use one
/// (Foundation §4.3). What is *not* structural, and is what this asserts, is that
/// all three actually reach the payload — Rating resolves by the triple, and an
/// artifact missing any leg of it is one no consumer can key on, however stable
/// the ids themselves are.
#[test]
fn every_projected_artifact_carries_the_resolution_triple() {
    let delta = PlanSubjectDelta {
        prices: vec![graduated_row()],
        ..shape_only()
    };
    let value = delta.to_value();

    assert_eq!(value.get("planId"), Some(&json!(plan_id().get())));
    assert_eq!(
        value.get("skuId"),
        Some(&json!(uuid::Uuid::from_u128(0x5_c1))),
        "the SKU leg is on the plan subject, not repeated per row"
    );
    for price in value["prices"].as_array().expect("the rows") {
        assert!(
            price.get("priceId").is_some_and(|id| !id.is_null()),
            "every row carries its own id: {price}"
        );
    }
}

// ---------------------------------------------------------------------------
// `inst-cr-return` (Slice 6) — the frozen set is stable for the pinned version.
// ---------------------------------------------------------------------------

/// The same delta renders byte-identically twice.
///
/// §2 step 3 promises a consumer "the frozen contract set, **stable** for the
/// pinned version", and a renderer with any non-deterministic member would break
/// that silently: the delta is written once into an INSERT-only store, but a
/// consumer re-reading it and a replay re-deriving it must agree. The hazard is
/// real and specific — `HashMap` iteration is seeded per process, so the grant
/// set's two maps and the per-phase map would each render in a different order in
/// two replicas. They are `BTreeMap` for exactly this reason, and this is what
/// says so.
#[test]
fn a_pinned_versions_contract_set_renders_identically_twice() {
    // **Two independently built deltas, not one rendered twice.** The claim
    // `inst-cr-return` needs is that a replica and a replay agree, and those are
    // two constructions of equal content rather than one value read twice — so a
    // render keyed on anything but content (an address, a construction order, a
    // counter) is what this shape can catch and the self-comparison cannot.
    //
    // It does **not** exercise the ordering hazard the paragraph above names:
    // that one is foreclosed by `BTreeMap` at the type level, this crate's
    // renderer holds no `HashMap` at all, and the fixture's maps are empty
    // anyway. The literal-JSON assertions elsewhere in this file are what pin the
    // rendered key order.
    let subject = || PlanSubjectDelta {
        prices: vec![graduated_row()],
        ..shape_only()
    };

    assert_eq!(
        subject().to_value().to_string(),
        subject().to_value().to_string(),
        "two constructions of one subject render the same bytes"
    );
}

#[test]
fn every_member_of_the_frozen_payload_is_classified_exactly_once() {
    // **What this still covers, after D-309 moved the rest to the compiler.**
    // `partition_delta_members` binds every field by name and hands each to
    // `named` exactly once, so a member added to `PlanSubjectDelta` is a compile
    // error (no rest pattern) and a member left out of both lists is an *unused
    // variable*, which this crate denies — measured: `error: unused variable`.
    //
    // What the compiler does **not** catch is a member classified **twice**: the
    // bindings are references and references are `Copy`, so handing one to `named`
    // in both lists compiles. That is what the dedup assertion below is for.
    //
    // The count is now a secondary reading rather than the guard. Its first
    // spelling was the guard and was armed backwards (D-303, corrected by D-309):
    // a forgotten member left the total at 22 and passed, while classifying it
    // correctly made 23 and failed — it fired on the fix and was silent on the
    // omission.
    let (reached, not_reached) = super::partition_delta_members(&shape_only());

    let mut all: Vec<&str> = reached.iter().chain(not_reached.iter()).copied().collect();
    let named = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(
        all.len(),
        named,
        "no member is classified on both sides: {all:?}"
    );
    assert_eq!(
        named, 23,
        "the payload's member count, read back: it moves with the payload on purpose, and a \
         member left unclassified is caught by the compiler before it reaches here"
    );

    // The two the roster genuinely reaches, through the guards that own them.
    assert_eq!(reached, vec!["prices", "change_contract"]);
    // And the one that is arguable is filed, not silently absent — D-162 clause
    // (5) is the reason, and it is written where the classification is made.
    assert!(
        not_reached.contains(&"composites"),
        "the composite set is classified rather than unstated: {not_reached:?}"
    );
}
