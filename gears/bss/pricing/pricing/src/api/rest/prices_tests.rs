//! The price views' wire shape, and the two distinctions a collapsing
//! serializer would destroy.
//!
//! Everything needing a store or a PDP is in `tests/rest_prices.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{
    IncludedAllowanceView, PriceContentView, PriceRowView, TierBandView, band_of, content_of,
};
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{AnchorDay, BillingAnchorPolicy, ProrationBasis};
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, RateMinor};
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{
    BandTop, IncludedAllowance, PriceRow, RolloverPolicy, TierBand, TierQualificationWindow,
};
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};

fn record(bands: Vec<TierBand>) -> PriceRecord {
    let key = ScopeKey::new(
        PlanId::new(Uuid::from_u128(0x91a4)),
        CurrencyCode::new("USD").expect("currency"),
        Region::new("EU").expect("region"),
        PhaseId::new(Uuid::from_u128(0x9ba5e)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Usage,
        Cohort::None,
    )
    .expect("key");
    PriceRecord {
        price_id: Uuid::from_u128(0x9_71ce),
        scope_key: key,
        row: PriceRow {
            bands,
            ..PriceRow::new(ChargeKind::Usage, None)
        },
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: None,
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Draft,
        created_by: Uuid::from_u128(0xac70),
        created_at_utc: utc_ymd_hms(2026, 8, 3, 9, 0, 0),
        row_version: RowVersion::new(2),
    }
}

fn body(view: &PriceRowView) -> serde_json::Value {
    serde_json::to_value(view).expect("the view serializes")
}

#[test]
fn an_open_top_band_is_a_null_upper_bound_and_round_trips_as_one() {
    // `BandTop` is an enum rather than an `Option<u64>` precisely because "open"
    // is a STATE of the band; the wire spells it `null`, and a round trip that
    // read `null` as "no bound given" would silently close the top band D-17
    // requires to be open.
    let open = TierBand::open(0, RateMinor::from_nano_minor(500).expect("rate"));
    let rendered = body(&PriceRowView::from(&record(vec![open])));
    assert!(
        rendered["content"]["bands"][0]["to_qty"].is_null(),
        "{rendered}"
    );

    let parsed = band_of(&TierBandView {
        from_qty: 0,
        to_qty: None,
        unit_price_nano_minor: 500,
    })
    .expect("an open band parses");
    assert_eq!(parsed.to_qty, BandTop::Open);
}

#[test]
fn a_closed_band_carries_its_exclusive_upper_bound() {
    let closed = TierBand::closed(0, 100, RateMinor::from_nano_minor(500).expect("rate"));
    let rendered = body(&PriceRowView::from(&record(vec![closed])));
    assert_eq!(
        rendered["content"]["bands"][0]["to_qty"],
        serde_json::json!(100),
        "{rendered}"
    );
}

#[test]
fn a_negative_unit_price_is_refused_rather_than_stored() {
    // Typed credit rows are deliberately out of scope, so a negative price is a
    // mistake and not an unsupported feature.
    let refusal = band_of(&TierBandView {
        from_qty: 0,
        to_qty: None,
        unit_price_nano_minor: -1,
    })
    .expect_err("a negative amount is refused");
    assert!(
        matches!(
            refusal,
            crate::domain::error::DomainError::AmountNegative(_)
        ),
        "{refusal:?}"
    );
}

/// A view with everything a well-formed `flat` row needs and nothing more.
fn clean_view() -> PriceContentView {
    PriceContentView {
        model_kind: Some("flat".to_owned()),
        amount_minor: Some(1_500),
        unit_rate_nano_minor: None,
        bands: None,
        package_size: None,
        package_price_minor: None,
        quantity_source: None,
        manual_quantity: None,
        meter: None,
        dimension_key: None,
        billing_granularity: None,
        tier_aggregation_window: None,
        tier_qualification_window: None,
        aggregation_function: None,
        aggregation_granularity: None,
        max_hold_granules: None,
        included_allowance: None,
        reserved_rate_nano_minor: None,
        reservation_flavor: None,
        min_qty_purchase: None,
        min_qty_usage: None,
        min_qty_usage_fallback: None,
        discount_ref: None,
        tax_inclusive: Some(false),
        tax_category_ref: None,
        billing_timing: None,
        billing_anchor_policy: None,
        anchor_day: None,
        proration_basis: None,
        credit_on_downgrade: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

#[test]
fn the_read_half_still_renders_both_slice_ten_primitives() {
    // The request half refuses them; the RESPONSE half must not drop them. The
    // domain model, the storage round trip and the D-129 supersession guard all
    // carry both fields, so a read that omitted them would lose a field that
    // guard compares between a predecessor and its successor - and a row can
    // hold either value from before the refusal, or from a path that is not this
    // surface.
    let mut stored = record(Vec::new());
    stored.row.tier_qualification_window = Some(TierQualificationWindow::TrailingPeriod);
    stored.row.included_allowance = Some(IncludedAllowance {
        quantity: 50,
        rollover_policy: RolloverPolicy::Carry,
    });

    let rendered = body(&PriceRowView::from(&stored));

    assert_eq!(
        rendered["content"]["tier_qualification_window"],
        serde_json::json!("trailing_period"),
        "{rendered}"
    );
    assert_eq!(
        rendered["content"]["included_allowance"]["quantity"],
        serde_json::json!(50),
        "{rendered}"
    );
    assert_eq!(
        rendered["content"]["included_allowance"]["rollover_policy"],
        serde_json::json!("carry"),
        "{rendered}"
    );
}

#[test]
fn the_request_half_refuses_what_slice_ten_has_not_landed_and_accepts_what_it_has() {
    // Delete `refuse_unlanded_primitives`' call in `content_of` and the two
    // refusal arms below stop failing - as does
    // `rest_prices.rs::a_create_carrying_a_tier_qualification_window_is_refused_at_any_value`.
    content_of(&clean_view()).expect("a row carrying neither primitive still converts");

    for window in ["trailing_period", "current"] {
        let refusal = content_of(&PriceContentView {
            tier_qualification_window: Some(window.to_owned()),
            ..clean_view()
        })
        .expect_err("an explicit window of ANY value is refused");
        assert!(
            matches!(
                &refusal,
                crate::domain::error::DomainError::InvalidRequest(detail)
                    if detail.contains("tier_qualification_window")
            ),
            "{refusal:?}"
        );
    }

    // `carry` alone: its artifact is a `pricing_plan_grant` row and there is no
    // such table, so an accepted declaration would freeze a horizon nothing
    // issues.
    let refusal = content_of(&PriceContentView {
        included_allowance: Some(IncludedAllowanceView {
            quantity: 100,
            rollover_policy: "carry".to_owned(),
        }),
        ..clean_view()
    })
    .expect_err("a carried allowance is refused");
    assert!(
        matches!(
            &refusal,
            crate::domain::error::DomainError::InvalidRequest(detail)
                if detail.contains("included_allowance") && detail.contains("carry")
        ),
        "{refusal:?}"
    );

    // And the positive control the whole slice exists for: `none` converts.
    // Without it the case above would pass identically against the old
    // field-wide refusal, which is the shape this change removed.
    let converted = content_of(&PriceContentView {
        included_allowance: Some(IncludedAllowanceView {
            quantity: 100,
            rollover_policy: "none".to_owned(),
        }),
        ..clean_view()
    })
    .expect("rolloverPolicy = none is authorable now: it compiles");
    assert_eq!(
        converted
            .row
            .included_allowance
            .expect("the declaration survives the conversion")
            .quantity,
        100
    );
}

#[test]
fn the_refusal_precedes_the_enum_spelling_check() {
    // Otherwise a caller who misspells the window is told the token is wrong and
    // infers the field is supported - and a caller who spells it right is told
    // nothing at all. The refusal is about the FIELD, so it runs first.
    let refusal = content_of(&PriceContentView {
        tier_qualification_window: Some("not_a_window".to_owned()),
        ..clean_view()
    })
    .expect_err("a misspelled window is refused too");
    let crate::domain::error::DomainError::InvalidRequest(detail) = &refusal else {
        panic!("{refusal:?}");
    };
    assert!(
        detail.contains("is not supported yet"),
        "the field's absence from the gear is the reason, not the token: {detail}"
    );
}

#[test]
fn the_view_names_the_rows_own_version_and_its_whole_key() {
    // D-141: the token is the price row's OWN version column, never derived from
    // the plan's - a per-row bulk conflict means nothing if every row of a plan
    // shares one version.
    let rendered = body(&PriceRowView::from(&record(Vec::new())));

    assert_eq!(rendered["row_version"], serde_json::json!(2));
    // **A second value, because one proves nothing.** `plans_tests` states the
    // discipline this file did not follow: a view that hardcoded the field would
    // pass against the value the fixture already carries. Two records at two
    // versions is what says the view reads the column.
    let mut older = record(Vec::new());
    older.row_version = RowVersion::new(1);
    assert_eq!(
        body(&PriceRowView::from(&older))["row_version"],
        serde_json::json!(1),
        "the view reads the row's version rather than a constant"
    );
    assert_eq!(
        rendered["scope_key"]["price_overlay"],
        serde_json::json!("base")
    );
    assert_eq!(
        rendered["scope_key"]["charge_kind"],
        serde_json::json!("usage")
    );
    assert!(rendered["scope_key"]["cohort"].is_null(), "{rendered}");
}

// ---------------------------------------------------------------------------
// The proration input contract at the boundary (`inst-pi-required`, §6).
// ---------------------------------------------------------------------------

/// A whole contract on the view.
fn with_contract(policy: &str, day: Option<u8>, basis: &str, credit: bool) -> PriceContentView {
    PriceContentView {
        billing_anchor_policy: Some(policy.to_owned()),
        anchor_day: day,
        proration_basis: Some(basis.to_owned()),
        credit_on_downgrade: Some(credit),
        ..clean_view()
    }
}

#[test]
fn a_whole_contract_reads_into_the_domain_value() {
    let content = content_of(&with_contract(
        "fixed_day",
        Some(15),
        "calendar_days_30",
        true,
    ))
    .expect("a well-formed contract converts");

    let contract = content
        .proration_contract
        .expect("the four fields produce one contract");
    assert_eq!(
        contract.billing_anchor_policy,
        BillingAnchorPolicy::FixedDay(AnchorDay::new(15).expect("a day of the month"))
    );
    assert_eq!(contract.proration_basis, ProrationBasis::CalendarDays30);
    assert!(contract.credit_on_downgrade);
}

/// Absence of the whole set is not this surface's refusal: a draft is allowed
/// to be unfinished, and `inst-pi-required` is what judges it at publish.
#[test]
fn an_absent_contract_leaves_the_draft_unfinished_rather_than_refused() {
    let content = content_of(&clean_view()).expect("an unfinished draft converts");
    assert!(content.proration_contract.is_none());
}

/// Two of three is a caller mistake this surface can name. Letting it through
/// would reach the publish rules as a *missing* contract and lose which field
/// the caller forgot.
#[test]
fn a_partial_contract_is_refused_naming_the_set() {
    let partial = PriceContentView {
        billing_anchor_policy: Some("calendar_month".to_owned()),
        proration_basis: Some("by_second".to_owned()),
        credit_on_downgrade: None,
        ..clean_view()
    };

    let refusal = content_of(&partial).expect_err("two of three is not a contract");

    assert!(
        format!("{refusal:?}").contains("must be sent together"),
        "{refusal:?}"
    );
}

/// The pairing, in both directions — the states [`BillingAnchorPolicy`] cannot
/// hold have to be refused here rather than silently dropped.
#[test]
fn the_anchor_day_pairing_is_refused_in_both_directions() {
    let day_without_fixed = with_contract("calendar_month", Some(9), "by_second", false);
    assert!(
        format!(
            "{:?}",
            content_of(&day_without_fixed).expect_err("a day beside calendar_month")
        )
        .contains("only authorable beside a fixed_day anchor")
    );

    let fixed_without_day = with_contract("fixed_day", None, "by_second", false);
    assert!(
        format!(
            "{:?}",
            content_of(&fixed_without_day).expect_err("a fixed_day with no day")
        )
        .contains("required beside a fixed_day anchor")
    );
}

/// A day the calendar has no room for is refused, and the message says why the
/// bound is 31 rather than 28 (K2's last-of-month clamp).
#[test]
fn an_anchor_day_outside_the_month_is_refused() {
    for day in [0_u8, 32] {
        let refusal = content_of(&with_contract("fixed_day", Some(day), "by_second", false))
            .expect_err("outside 1..=31");
        assert!(
            format!("{refusal:?}").contains("between 1 and 31"),
            "{refusal:?}"
        );
    }
}

/// K1's set is closed, and a token outside it is named against the set rather
/// than stored — the enum-drift class this slice owns the enum to kill.
#[test]
fn a_basis_outside_the_canonical_set_is_refused_naming_the_set() {
    let refusal = content_of(&with_contract(
        "calendar_month",
        None,
        "calendar_days_31",
        false,
    ))
    .expect_err("not one of K1's five");

    let rendered = format!("{refusal:?}");
    assert!(rendered.contains("calendar_days_actual"), "{rendered}");
    assert!(rendered.contains("by_second"), "{rendered}");
}

/// D-312 — what the authoring write refuses, and what it must keep accepting.
///
/// The second half is the load-bearing one. This change is one careless `if` away
/// from being "the write plane validates rows", which is the design §4.2 exists to
/// prevent, and the only thing that would report it is a case asserting that an
/// incomplete row still saves.
mod key_contradictions {
    use super::*;
    use crate::domain::rules::MODEL_KIND_CHARGEKIND_MISMATCH;

    fn key_on(charge_kind: ChargeKind) -> ScopeKey {
        ScopeKey::new(
            PlanId::new(Uuid::from_u128(0x91a4)),
            CurrencyCode::new("USD").expect("currency"),
            Region::new("EU").expect("region"),
            PhaseId::new(Uuid::from_u128(0x9ba5e)),
            PriceEligibility::AllSubscriptions,
            charge_kind,
            Cohort::None,
        )
        .expect("key")
    }

    fn check(charge_kind: ChargeKind, view: &PriceContentView) -> Result<(), String> {
        let content = content_of(view).expect("the view converts");
        super::super::require_no_key_contradiction(&key_on(charge_kind), &content)
            .map_err(|e| format!("{e:?}"))
    }

    #[test]
    fn a_flat_row_on_a_usage_key_is_refused_at_the_write() {
        let refusal = check(ChargeKind::Usage, &clean_view()).expect_err("refused");
        assert!(
            refusal.contains(MODEL_KIND_CHARGEKIND_MISMATCH),
            "expected the matrix code, got: {refusal}"
        );
    }

    #[test]
    fn the_same_row_on_a_recurring_key_is_accepted() {
        // The identical content. Only the frozen half of the pair moved, which is
        // what makes the case above about the key rather than about `flat`.
        check(ChargeKind::Recurring, &clean_view()).expect("a flat recurring row is legal");
    }

    /// D-45's allowance joins this category (`ALLOWANCE_ON_NON_USAGE`).
    ///
    /// The declaration and the frozen `chargeKind` are both in the request, and
    /// no later call moves the key, so the write refuses rather than storing a
    /// row only publish could reject. The mirror case is the same content on a
    /// usage key, which converts and is judged by the rest of `inst-ac-gate`.
    #[test]
    fn an_allowance_on_a_non_usage_key_is_refused_at_the_write() {
        let declared = PriceContentView {
            model_kind: Some("per_unit".to_owned()),
            amount_minor: None,
            unit_rate_nano_minor: Some(2_000_000_000),
            meter: Some("egress.gb".to_owned()),
            billing_granularity: Some("whole_unit".to_owned()),
            included_allowance: Some(IncludedAllowanceView {
                quantity: 100,
                rollover_policy: "none".to_owned(),
            }),
            ..clean_view()
        };

        let refusal = check(ChargeKind::Recurring, &declared).expect_err("refused");
        assert!(
            refusal.contains("ALLOWANCE_ON_NON_USAGE"),
            "expected the allowance gate's key code, got: {refusal}"
        );

        check(ChargeKind::Usage, &declared)
            .expect("the identical content on a usage key is exactly what the compile admits");
    }

    #[test]
    fn a_graduated_usage_row_is_not_judged_against_the_placeholder_charge_kind() {
        // `content_of` builds its row with `ChargeKind::Recurring` as a placeholder.
        // Judge that instead of the key and `graduated` reads as a recurring band
        // row — refused, and every legitimate metered price with it. This case is
        // the reason the helper takes the key at all.
        let view = PriceContentView {
            model_kind: Some("graduated".to_owned()),
            amount_minor: None,
            bands: Some(vec![TierBandView {
                from_qty: 0,
                to_qty: None,
                unit_price_nano_minor: 1_000_000_000,
            }]),
            billing_granularity: Some("whole_unit".to_owned()),
            tier_aggregation_window: Some("calendar_month".to_owned()),
            ..clean_view()
        };
        check(ChargeKind::Usage, &view).expect("a graduated usage row is legal");
    }

    #[test]
    fn an_eval_policy_field_on_a_recurring_key_is_refused_at_the_write() {
        let view = PriceContentView {
            billing_granularity: Some("whole_unit".to_owned()),
            ..clean_view()
        };
        check(ChargeKind::Recurring, &view).expect_err("billingGranularity is usage-row only");
    }

    #[test]
    fn a_row_with_no_model_kind_still_saves() {
        // Absence. The kind arrives in a later call, and refusing this is exactly
        // the multi-call authoring §4.2 protects.
        let view = PriceContentView {
            model_kind: None,
            amount_minor: None,
            ..clean_view()
        };
        check(ChargeKind::Usage, &view).expect("an unfinished row is authorable");
        check(ChargeKind::Recurring, &view).expect("an unfinished row is authorable");
    }

    #[test]
    fn a_priced_row_with_no_price_still_saves() {
        // Also absence, and the one most likely to be swept in: the kind and the
        // charge kind agree, only the money has not been typed yet.
        let view = PriceContentView {
            model_kind: Some("flat".to_owned()),
            amount_minor: None,
            ..clean_view()
        };
        check(ChargeKind::Recurring, &view).expect("an unpriced draft is authorable");
    }

    #[test]
    fn a_graduated_row_with_no_bands_yet_still_saves() {
        let view = PriceContentView {
            model_kind: Some("graduated".to_owned()),
            amount_minor: None,
            bands: None,
            ..clean_view()
        };
        check(ChargeKind::Usage, &view).expect("a ladder is laid over several calls");
    }
}

/// **What the read half emits, the write half accepts** — the round trip
/// `PriceContentView` being one type on both halves promises, and which nothing
/// asserted.
///
/// The accept half's only fixture is `clean_view()`, a hand-written literal that
/// sets `bands: None`. The **producer** populates `bands: Some(...)` on every row
/// at `PriceContentView::from`, so a request-half case starting from that literal
/// starts from a value the response half cannot emit, and exercises no round trip
/// at all. A literal the producer cannot produce is a fixture that has stopped
/// being about the thing it names. (`dimension_key` is not in that class: the
/// producer emits `None` for an undimensioned line, which is what the literal
/// carries.)
///
/// Starting from the producer is what makes the case falsifiable, and it is red the
/// moment a stored row carries either Slice-10 primitive: `refuse_unlanded_primitives`
/// is `content_of`'s first statement and refuses **any** request whose
/// `tier_qualification_window.is_some()`, while the emit half renders that column
/// for any row that holds one. The two halves were pinned in contradiction in this
/// one file, by two tests that never met.
#[test]
fn every_row_the_read_half_can_emit_is_one_the_write_half_accepts() {
    // A bare row first: the plain round trip, which must hold unconditionally.
    let plain = record(Vec::new());
    content_of(&PriceContentView::from(&plain))
        .expect("the write half accepts what the read half emitted");

    // A banded row: `bands: Some(vec![...])` is the shape the producer always
    // emits and the old fixture never had.
    let banded = record(vec![
        TierBand::closed(
            0,
            100,
            RateMinor::from_nano_minor(40_500_000).expect("a non-negative rate"),
        ),
        TierBand {
            from_qty: 100,
            to_qty: BandTop::Open,
            unit_price_rate: RateMinor::from_nano_minor(10_125_000).expect("a non-negative rate"),
        },
    ]);
    let round_tripped =
        content_of(&PriceContentView::from(&banded)).expect("a banded row round-trips");
    assert_eq!(
        round_tripped.row.bands.len(),
        2,
        "and the bands survive the trip rather than being dropped on the way in"
    );
}

/// **The two Slice-10 primitives are the exception, and it is stated rather than
/// discovered.**
///
/// A row that carries either cannot be `PATCH`ed with its own representation:
/// `refuse_unlanded_primitives` refuses the request half while the read half
/// renders the column. The contradiction is **armed rather than firing** — neither
/// REST writer can set either column, both going through `content_of` — and it
/// fires the moment a row acquires one through a path that is not this surface:
/// the bulk import, a migration, `infra::synthesis`, or the day Slice 10 lands the
/// write half.
///
/// Asserted rather than left implicit, because the sibling above would otherwise
/// have to be weakened to a subset of rows the day it started failing, and this is
/// the case that says which subset and why.
#[test]
fn a_row_carrying_an_unlanded_primitive_cannot_be_written_back_and_that_is_known() {
    for (what, mutate) in [
        (
            "tier_qualification_window",
            (|row: &mut PriceRow| {
                row.tier_qualification_window = Some(TierQualificationWindow::TrailingPeriod);
            }) as fn(&mut PriceRow),
        ),
        ("included_allowance", |row: &mut PriceRow| {
            row.included_allowance = Some(IncludedAllowance {
                quantity: 50,
                rollover_policy: RolloverPolicy::Carry,
            });
        }),
    ] {
        let mut stored = record(Vec::new());
        mutate(&mut stored.row);

        // The read half renders it...
        let emitted = PriceContentView::from(&stored);
        // ...and the write half refuses exactly that value back.
        let refusal = content_of(&emitted)
            .expect_err("the write half refuses the primitive the read half just emitted");
        assert!(
            matches!(
                &refusal,
                crate::domain::error::DomainError::InvalidRequest(detail) if detail.contains(what)
            ),
            "{what}: {refusal:?}"
        );
    }
}

/// A blank `tax_category_ref` is refused, **in a sentence a reader can read**.
///
/// The refusal text is returned verbatim in the Problem detail, and it carried a
/// twenty-space run mid-sentence until 2026-08-20 — a `\` line continuation
/// dropped out of the literal, so every operator who tripped the door was shown a
/// visibly broken message. Neither rustfmt nor clippy inspects string contents, so
/// this assertion is the only gate that can see it; it is written against the
/// rendered sentence rather than the literal, which is what a caller receives.
#[test]
fn the_blank_tax_category_refusal_reads_as_one_sentence() {
    let refusal = content_of(&PriceContentView {
        tax_category_ref: Some("   ".to_owned()),
        ..clean_view()
    })
    .expect_err("a blank category is a caller mistake naming one field");

    let crate::domain::error::DomainError::InvalidRequest(detail) = &refusal else {
        panic!("a blank category is InvalidRequest, not {refusal:?}");
    };
    assert!(
        detail.starts_with("content.tax_category_ref must not be blank"),
        "{detail}"
    );
    assert!(
        !detail.contains("  "),
        "the detail carries a run of spaces, so the sentence renders broken: {detail:?}"
    );
    assert!(
        detail.contains("omit it to state no category, which resolves the region's default"),
        "{detail}"
    );
}
