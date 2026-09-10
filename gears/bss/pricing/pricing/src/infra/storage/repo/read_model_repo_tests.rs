//! What the frozen payload reader reads, and what it refuses.
//!
//! **No database here on purpose.** [`sellability_facts`] is a pure function of a
//! [`StoredDelta`], and the property that matters is that it is the inverse of
//! [`PlanSubjectDelta::to_value`] on every field it touches — which is stated most
//! sharply by rendering a delta and reading it straight back.
//! `tests/sqlite_sellability.rs` is where [`delta_at`] is proved *through the
//! store*, against the resolution rule and the two-version freeze that need rows.

use bss_pricing_sdk::CatalogVersion;

use serde_json::json;
use std::collections::BTreeMap;
use uuid::Uuid;

use super::{StoredDelta, sellability_facts};
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{EntitlementGrants, PlanChangeContract};
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount};
use crate::domain::plan_shape::{
    BillingCycle, CompositeMeter, CustomIntervalUnit, DescriptorSet, Frequency, PeriodFloorCap,
    PhaseKind, PlanPhase,
};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{ModelKind, PriceRow};
use crate::domain::projection::PlanSubjectDelta;
use crate::domain::scope_key::{
    ChargeKind, Cohort, DimensionKey, Meter, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::sellability::{PinnedFacts, SellabilityFacts};
use crate::domain::window::{KeyWindows, WindowInterval, WindowState};
use crate::infra::storage::RepoError;
use time::OffsetDateTime;

/// `2099-01-01T00:00:00Z` plus `day` whole days.
fn at(day: i64) -> OffsetDateTime {
    utc_ymd_hms(2099, 1, 1, 0, 0, 0) + time::Duration::days(day)
}

fn plan_id() -> PlanId {
    PlanId::new(Uuid::from_u128(0x9_1a4))
}

fn phase() -> PhaseId {
    PhaseId::new(Uuid::from_u128(0xfa_5e))
}

fn key_of(charge_kind: ChargeKind, eligibility: PriceEligibility, cohort: Cohort) -> ScopeKey {
    ScopeKey::new(
        plan_id(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        phase(),
        eligibility,
        charge_kind,
        cohort,
    )
    .expect("the class pairs with the cohort")
}

fn row_on(scope_key: ScopeKey) -> PriceRecord {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(1_200).expect("a non-negative amount"));
    PriceRecord {
        price_id: Uuid::from_u128(0xb_0001),
        scope_key,
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: None,
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Published,
        created_by: Uuid::from_u128(0xac_10),
        created_at_utc: at(0),
        row_version: RowVersion::new(0),
    }
}

/// A delta populated in every member the reader touches, and in the members it
/// does not, so nothing here passes because a field was left at its default.
///
/// Two keys — one `all_subscriptions` recurring row and one grandfathered
/// generation whose cohort is a real instant — because the cohort axis is the one
/// the reader has to rebuild rather than default, and a payload with only
/// `cohort: null` would never exercise it.
fn populated() -> PlanSubjectDelta {
    let everyone = key_of(
        ChargeKind::Recurring,
        PriceEligibility::AllSubscriptions,
        Cohort::None,
    );
    let generation = key_of(
        ChargeKind::Recurring,
        PriceEligibility::ExistingGrandfathered,
        Cohort::Generation(at(1)),
    );
    // **A third key, and it carries a usage line** (D-196): the round-trip pin
    // below compares every projected key, so a corpus of line-less keys would
    // leave axes 9 and 10 asserted by nothing — the projector could stop writing
    // them and the reader stop reading them, in step, with this file green.
    let metered = key_of(
        ChargeKind::Usage,
        PriceEligibility::AllSubscriptions,
        Cohort::None,
    )
    .with_usage_line(
        Some(Meter::new("cloudlets").expect("a non-blank meter")),
        DimensionKey::new("region=eu"),
    )
    .expect("a usage key carries its line");
    PlanSubjectDelta {
        plan_id: plan_id(),
        revision: 3,
        lifecycle_state: LifecycleState::Retired,
        sku_id: Some(Uuid::from_u128(0x5_c1)),
        plan_tier: Some("gold".to_owned()),
        plan_tier_override: true,
        billing_cycle: Some(BillingCycle::Recurring),
        frequency: Some(Frequency::CustomEveryN {
            n: 7,
            unit: CustomIntervalUnit::Months,
        }),
        available_from: Some(at(2)),
        available_to: Some(at(400)),
        purchase_min_qty: Some(1),
        purchase_max_qty: Some(9),
        invoice_grouping_key: Some("bundle-a".to_owned()),
        phases: vec![PlanPhase {
            phase_id: phase(),
            kind: PhaseKind::Evergreen,
            display_name: None,
            ordinal: 0,
            converts_to_phase_id: None,
            phase_duration_days: None,
            display_trial_days: None,
        }],
        addon_rules: Vec::new(),
        descriptor_set: Some(DescriptorSet {
            invoice_line_template: Some("{plan}".to_owned()),
            gl_code: Some("4000".to_owned()),
            itemization_rule: Some("per_charge".to_owned()),
            additional: std::collections::BTreeMap::new(),
        }),
        // Populated for the same reason as the composite set below: an empty
        // list would let `period_floor_cap_value` render nothing and still pass
        // the key census, so the market pair, the floor and the absent cap are
        // all exercised (D-319).
        period_floor_caps: vec![PeriodFloorCap {
            currency: CurrencyCode::new("USD").expect("three letters"),
            region: Region::new("us").expect("non-blank"),
            floor_minor: Some(MinorAmount::new(50_000).expect("non-negative")),
            cap_minor: None,
        }],
        // Populated rather than left empty, on this fixture's own rule: a member
        // at its default proves nothing about the renderer that writes it, and
        // `composite_value` walks a formula it never reads (A4) - so an empty list
        // would exercise no part of it.
        composites: vec![CompositeMeter {
            composite_id: Uuid::from_u128(0xc0_11),
            output_unit: "vm".to_owned(),
            constituent_units: vec!["vcpu".to_owned(), "ram".to_owned()],
            formula: serde_json::json!({ "op": "weighted_sum", "w": [1, 2] }),
        }],
        entitlement_grants: EntitlementGrants::default(),
        change_contract: PlanChangeContract::default(),
        prices: vec![
            row_on(everyone.clone()),
            row_on(generation.clone()),
            row_on(metered),
        ],
        tax_projection: BTreeMap::new(),
        windows: vec![
            KeyWindows {
                scope_key: everyone,
                intervals: vec![
                    WindowInterval::new(at(0), Some(at(5)), WindowState::Expired),
                    WindowInterval::new(at(5), None, WindowState::Active),
                ],
            },
            KeyWindows {
                scope_key: generation,
                intervals: vec![WindowInterval::new(
                    at(1),
                    Some(at(9)),
                    WindowState::Scheduled,
                )],
            },
        ],
    }
}

fn stored(payload: serde_json::Value) -> StoredDelta {
    StoredDelta {
        catalog_version: CatalogVersion::new(11),
        projected_at: at(3),
        payload,
    }
}

fn pinned_of(delta: &PlanSubjectDelta) -> PinnedFacts {
    match sellability_facts(&stored(delta.to_value())).expect("a payload this gear wrote") {
        SellabilityFacts::Pinned(pinned) => pinned,
        SellabilityFacts::NotAddressable { .. } => {
            panic!("a payload that exists is a version that carries the plan")
        }
    }
}

/// **The narrow reader's one risk, paid.** A payload key renamed on one side only
/// is invisible to a reader that never asserts the pair, so every field the reader
/// reads is compared against the delta it was rendered from — a spelling change in
/// either direction reddens here.
///
/// Field by field rather than through a `PartialEq` on the whole: the reader
/// deliberately reads *less* than the delta carries, so an equality over the two
/// types is not the property. What is asserted is that each fact survived the round
/// trip; the members it ignores are enumerated in
/// [`the_payloads_members_partition_into_the_read_and_the_ignored`], so the pair of
/// tests is the whole of the payload.
#[test]
fn every_field_the_reader_reads_round_trips_through_the_payload() {
    let delta = populated();

    let facts = pinned_of(&delta);

    assert_eq!(facts.plan_id, delta.plan_id);
    assert_eq!(facts.catalog_version, CatalogVersion::new(11));
    assert_eq!(facts.lifecycle_state, delta.lifecycle_state);
    assert_eq!(facts.available_from, delta.available_from);
    assert_eq!(facts.available_to, delta.available_to);
    assert_eq!(
        facts.frequency, delta.frequency,
        "a custom frequency's interval is rebuilt from the payload's own n and unit, not left at \
         the ALL member's placeholder"
    );
    assert_eq!(
        facts.price_keys,
        delta
            .prices
            .iter()
            .map(|record| record.scope_key.clone())
            .collect::<Vec<_>>(),
        "every axis of every projected key survives, cohort included"
    );
    assert_eq!(
        facts.windows, delta.windows,
        "the intervals and their states come back as the version froze them"
    );
}

/// **The other half of the pair: every member the narrow reader ignores, named.**
///
/// The round-trip case above justified itself by saying the ignored fields were
/// "listed in the case below" — and no case listed them. That clause is what
/// licenses a reader narrower than the payload, so with nothing carrying it a
/// maintainer who added a delta member a predicate ought to read had **no guard**
/// and was told one existed. A member in neither list reddens here.
///
/// The partition is over the **payload** and not over [`PlanSubjectDelta`]:
/// `evaluationPolicyVersion` and `crossBoundaryChangePolicy` are constants
/// `to_value` stamps on and are fields of no struct, so the exhaustive destructure
/// that makes the renderer safe cannot see them and neither would a list derived
/// from it.
///
/// Sorted on both sides rather than compared in place, because `serde_json`'s map
/// ordering is a build feature and not a fact about the payload.
#[test]
fn the_payloads_members_partition_into_the_read_and_the_ignored() {
    /// Every member [`sellability_facts`] reads.
    const READ: &[&str] = &[
        "availableFrom",
        "availableTo",
        "frequency",
        "lifecycleState",
        "planId",
        "prices",
        "windows",
    ];
    /// Every member it deliberately does not.
    const IGNORED: &[&str] = &[
        "addonRules",
        // Slice 6's plan-change contract. **Ignored deliberately**: the six
        // sellability predicates ask whether a thing may be *sold*, and these
        // three say where a subscription may *move* once sold. A plan offering
        // no self-service change is perfectly sellable, and one offering it is
        // not thereby sellable — so reading them here would make an
        // authorization fact decide a sellability one.
        "allowedChangeTargets",
        "billingCycle",
        "comparabilityRank",
        // Slice 10's derived-meter definitions. **Ignored deliberately**: a
        // composite says how a billable quantity is *derived* once a thing is
        // sold, and the six predicates ask whether it may be sold at all. A plan
        // defining no composite is sellable and one defining three is not thereby
        // sellable - the same reading `entitlementGrants` and the change contract
        // get, three members down.
        "composites",
        "crossBoundaryChangePolicy",
        "descriptorSet",
        // Slice 6's entitlement grant set and its materialized map. **Ignored
        // deliberately**, for the change contract's reason one line up: the six
        // predicates ask whether a thing may be *sold*, and these say what a
        // subscriber may *use* once it is. A plan granting nothing is still
        // sellable.
        "entitlementGrants",
        "evaluationPolicyVersion",
        "invoiceGroupingKey",
        // D-319's plan-level period floor/cap. **Ignored deliberately**: a
        // period bound is what a thing costs at minimum once sold, and the six
        // predicates ask whether it may be sold at all. A plan carrying no
        // minimum is sellable and one carrying a $500 minimum is not thereby
        // unsellable - `entitlementGrants`' reading, one member over.
        "periodFloorCaps",
        "phaseGrantMap",
        "phases",
        "planTier",
        "planTierOverride",
        "purchaseMaxQty",
        "purchaseMinQty",
        "revision",
        "skuId",
        "usageCounterOnPlanChange",
    ];

    let payload = populated().to_value();
    let mut present: Vec<&str> = payload
        .as_object()
        .expect("the payload is an object")
        .keys()
        .map(String::as_str)
        .collect();
    present.sort_unstable();

    let mut accounted: Vec<&str> = READ.iter().chain(IGNORED).copied().collect();
    accounted.sort_unstable();

    assert_eq!(
        present, accounted,
        "every payload member is either read by `sellability_facts` or deliberately \
         ignored by it, and a new one is neither until somebody decides which"
    );
}

/// The version is the **store's** fact and not the payload's, which is why it is a
/// parameter of the reader rather than a member it looks for.
///
/// `PlanSubjectDelta` carries a `revision` and no `catalogVersion`: the version is
/// the row's column, so a reader that took it from the payload would be reading a
/// field the writer never wrote.
#[test]
fn the_version_comes_from_the_row_and_not_from_the_payload() {
    let delta = populated();

    let facts = pinned_of(&delta);

    assert_eq!(facts.catalog_version, CatalogVersion::new(11));
    assert_eq!(
        delta.to_value().get("catalogVersion"),
        None,
        "the payload has no such member, so there is nothing to read it from"
    );
}

/// A custom frequency is the one variant whose payload rides two extra members,
/// and reading the token alone would compute a one-day margin for whatever the
/// plan authored.
///
/// Stated on its own because `Frequency::ALL`'s custom member is a **variant
/// representative** carrying `CUSTOM_INTERVAL_PLACEHOLDER`, so a reader that
/// stopped at the token would have found a legal-looking value.
#[test]
fn a_custom_frequency_is_rebuilt_from_its_own_interval_and_not_from_the_placeholder() {
    let delta = PlanSubjectDelta {
        frequency: Some(Frequency::CustomEveryN {
            n: 5,
            unit: CustomIntervalUnit::Days,
        }),
        ..populated()
    };

    assert_eq!(
        pinned_of(&delta).frequency,
        Some(Frequency::CustomEveryN {
            n: 5,
            unit: CustomIntervalUnit::Days
        })
    );
    assert_ne!(
        Frequency::CUSTOM_INTERVAL_PLACEHOLDER,
        5,
        "and the value asserted above is not what the placeholder would have given"
    );
}

/// An absent optional instant and a `null` one read alike: `serde_json` renders
/// `None` as `null`, so distinguishing them would be reading the serializer rather
/// than the fact.
#[test]
fn an_absent_optional_instant_and_a_null_one_read_alike() {
    let mut payload = populated().to_value();
    let object = payload.as_object_mut().expect("the payload is an object");
    object.insert("availableFrom".to_owned(), serde_json::Value::Null);
    object.remove("availableTo");

    let facts = match sellability_facts(&stored(payload)).expect("both spellings are readable") {
        SellabilityFacts::Pinned(pinned) => pinned,
        SellabilityFacts::NotAddressable { .. } => panic!("a payload exists"),
    };

    assert_eq!(facts.available_from, None);
    assert_eq!(facts.available_to, None);
}

/// A payload this gear could not have written is a **corrupt row**, not a bad
/// request: the writer is this gear, so a member that is absent, of the wrong type
/// or outside its enumeration means the reader and the writer disagree about the
/// vocabulary, and no caller can reshape the request to fix it.
///
/// Five mutilations, one per class of disagreement, because a single case would
/// have proved only the class it belonged to.
#[test]
fn a_payload_this_gear_could_not_have_written_is_a_corrupt_row() {
    let mutilate = |edit: fn(&mut serde_json::Value)| {
        let mut payload = populated().to_value();
        edit(&mut payload);
        sellability_facts(&stored(payload)).expect_err("the reader must refuse")
    };

    // A member the predicates read, removed.
    let absent = mutilate(|payload| {
        payload
            .as_object_mut()
            .expect("object")
            .remove("lifecycleState");
    });
    assert!(
        matches!(absent, RepoError::CorruptRow(ref detail) if detail.contains("lifecycleState"))
    );

    // A token outside the enumeration its column is constrained to.
    let alien_state = mutilate(|payload| {
        payload
            .as_object_mut()
            .expect("object")
            .insert("lifecycleState".to_owned(), json!("paused"));
    });
    assert!(matches!(alien_state, RepoError::CorruptRow(ref detail) if detail.contains("paused")));

    // A scope-key axis on a plane the authoring path cannot write. `ScopeKey::new`
    // answers `base` for everything, so an unread overlay would be silently
    // flattened - `price_repo::to_scope_key`'s own reason for asking.
    let alien_overlay = mutilate(|payload| {
        payload["windows"][0]["scopeKey"]["priceOverlay"] = json!("partner");
    });
    assert!(
        matches!(alien_overlay, RepoError::CorruptRow(ref detail) if detail.contains("partner"))
    );

    // The cohort / eligibility biconditional, re-established on this rehydration
    // exactly as the store re-establishes it on its own: a grandfathered class
    // with no generation is not a key this gear can have authored.
    let unpaired = mutilate(|payload| {
        payload["prices"][1]["scopeKey"]["cohort"] = serde_json::Value::Null;
    });
    assert!(matches!(unpaired, RepoError::CorruptRow(_)));

    // A window state the **column** admits and the **projector** never writes.
    // `chk_pricing_price_window_state` allows all four tokens, but
    // `PROJECTED_WINDOW_STATES` is the three the renderer emits - `cancelled` is a
    // schedule that never happened - so this is a payload no version of this gear
    // produced. Read against `WindowState::ALL` it was accepted silently, and
    // stayed invisible because `covers_at` and `coverage_end` drop `cancelled`
    // again downstream: the answer was fail-closed and the disagreement unreported.
    let unprojectable = mutilate(|payload| {
        payload["windows"][0]["intervals"][0]["state"] = json!("cancelled");
    });
    assert!(
        matches!(unprojectable, RepoError::CorruptRow(ref detail) if detail.contains("cancelled")),
        "a state the projector filters out is a corrupt row: {unprojectable:?}"
    );
}
