//! Unit tests for the `PriceOverlay` vocabulary.
//!
//! Most of what this module owes is proved by **not compiling**, so the tests
//! here cover the two things a type system cannot state: the class-specificity
//! *order* (a rank, and it must be the one `inst-plv-class-tiebreak` publishes)
//! and the resolution rule `inst-plv-lines` declares normative.

use super::*;
use crate::domain::instant::utc_ymd_hms;
use time::OffsetDateTime;

fn plan(n: u128) -> PlanId {
    PlanId::new(Uuid::from_u128(n))
}

fn at(year: i32) -> OffsetDateTime {
    utc_ymd_hms(year, 1, 1, 0, 0, 0)
}

fn line(key: LineKey, bp: i64) -> OverlayLine {
    OverlayLine {
        line_id: Uuid::from_u128(u128::from(bp.unsigned_abs())),
        key,
        adjustment: Adjustment::Discount(Magnitude::PercentBp(bp)),
    }
}

// ---------------------------------------------------------------------------
// The class order.
// ---------------------------------------------------------------------------

/// The derived `Ord` **is** `inst-plv-class-tiebreak`'s published order:
/// `customerGroup > partner > orgTier > brand > region > global`.
///
/// Asserted rather than assumed because the variants are declared *reversed* on
/// purpose — least specific first — so that no later code can build a second,
/// disagreeing ranking out of this type. A reordering of the declaration would
/// silently invert the tie-break Tariffs is told to adopt verbatim.
#[test]
fn the_class_order_ranks_customer_group_highest_and_global_lowest() {
    let mut ranked = ScopeClass::ALL.to_vec();
    ranked.sort_unstable();
    assert_eq!(
        ranked,
        vec![
            ScopeClass::Global,
            ScopeClass::Region,
            ScopeClass::Brand,
            ScopeClass::OrgTier,
            ScopeClass::Partner,
            ScopeClass::CustomerGroup,
        ],
        "the derived Ord must be the published class-specificity order"
    );
    assert!(ScopeClass::CustomerGroup > ScopeClass::Partner);
    assert!(ScopeClass::Region > ScopeClass::Global);
}

/// Every class round-trips through its token, and `org_tier` is `snake_case`
/// on the wire as well as in the column — `toolkit_macros::api_dto` does not
/// rename, so `orgTier` survives only in the design set's prose.
#[test]
fn every_class_round_trips_through_its_token() {
    for class in ScopeClass::ALL {
        assert_eq!(ScopeClass::parse(class.as_str()), Some(*class));
    }
    assert_eq!(ScopeClass::OrgTier.as_str(), "org_tier");
    assert_eq!(ScopeClass::parse("orgTier"), None);
}

/// Only `global` has no value universe; the other five each name a table.
#[test]
fn only_the_classless_scope_has_no_taxonomy() {
    assert_eq!(ScopeClass::Global.taxonomy_table(), None);
    for class in ScopeClass::ALL.iter().filter(|c| **c != ScopeClass::Global) {
        assert!(
            class.taxonomy_table().is_some(),
            "{class} must name the universe `inst-plv-scope` validates it against"
        );
    }
}

// ---------------------------------------------------------------------------
// The scope pairing, which is unrepresentable rather than checked.
// ---------------------------------------------------------------------------

/// A classless overlay carrying a value cannot be built.
#[test]
fn the_classless_scope_cannot_carry_a_value() {
    let value = ScopeValue::new("everyone").expect("a non-blank value");
    assert_eq!(
        ScopeSelector::scoped(ScopeClass::Global, value),
        None,
        "the biconditional `chk_pricing_price_overlay_scope_value` has no representable counterexample"
    );
}

/// ...and a scoped overlay always has one, which the store reads back as a
/// non-empty string.
#[test]
fn a_scoped_overlay_stores_a_non_empty_value_and_the_classless_one_stores_empty() {
    let value = ScopeValue::new("acme").expect("a non-blank value");
    let scoped = ScopeSelector::scoped(ScopeClass::Brand, value).expect("brand is not global");
    assert_eq!(scoped.stored_value(), "acme");
    assert_eq!(ScopeSelector::Global.stored_value(), "");
    assert_eq!(ScopeSelector::Global.class(), ScopeClass::Global);
}

/// A blank value is refused, which is what keeps the store's `''` sentinel
/// unforgeable.
#[test]
fn a_blank_scope_value_is_refused() {
    assert_eq!(ScopeValue::new(""), None);
    assert_eq!(ScopeValue::new("   "), None);
    assert_eq!(TargetSku::new(""), None);
}

// ---------------------------------------------------------------------------
// The line key, and the two pairings it makes unrepresentable.
// ---------------------------------------------------------------------------

/// A `cohort` on the list-default line cannot be built — §6's
/// `CHECK (cohort IS NULL OR plan_id IS NOT NULL)` with no `CHECK`.
#[test]
fn a_cohort_cannot_narrow_the_list_default_line() {
    assert_eq!(LineKey::list_default().for_cohort(at(2099)), None);
    assert!(LineKey::for_plan(plan(1)).for_cohort(at(2099)).is_some());
}

/// The specificity rank is `(plan, sku)` > `(plan)` > default, and **`cohort` is
/// not an input to it** (D-78: a filter, not a level).
#[test]
fn cohort_does_not_change_a_lines_specificity() {
    let sku = TargetSku::new("sku-a").expect("a non-blank sku");
    assert_eq!(LineKey::list_default().specificity(), 0);
    assert_eq!(LineKey::for_plan(plan(1)).specificity(), 1);
    assert_eq!(LineKey::for_sku(plan(1), sku).specificity(), 2);

    let plain = LineKey::for_plan(plan(1));
    let cohorted = plain.clone().for_cohort(at(2099)).expect("plan is named");
    assert_eq!(
        plain.specificity(),
        cohorted.specificity(),
        "the eligibility filter must not rank lines against each other"
    );
}

// ---------------------------------------------------------------------------
// D-78's eligibility filter.
// ---------------------------------------------------------------------------

/// A `cohort`-less line does **not** apply to a grandfathered row.
///
/// This is the whole of D-78: before it, a single `+2000 bp` markup repriced a
/// cohort whose price the ADR-0002 machinery exists to guarantee.
#[test]
fn a_cohort_less_line_is_not_eligible_for_a_grandfathered_row() {
    let key = LineKey::for_plan(plan(1));
    assert!(key.eligible_for(PriceEligibility::AllSubscriptions, None));
    assert!(key.eligible_for(PriceEligibility::NewSubscriptionsOnly, None));
    assert!(
        !key.eligible_for(PriceEligibility::ExistingGrandfathered, Some(at(2099))),
        "a grandfathered generation is exempt from every line whose cohort is unset"
    );
}

/// A `cohort`-carrying line applies **only** to its own generation.
#[test]
fn a_cohort_line_is_eligible_for_its_generation_and_nothing_else() {
    let key = LineKey::for_plan(plan(1))
        .for_cohort(at(2099))
        .expect("plan is named");

    assert!(key.eligible_for(PriceEligibility::ExistingGrandfathered, Some(at(2099))));
    assert!(
        !key.eligible_for(PriceEligibility::ExistingGrandfathered, Some(at(2098))),
        "a sibling generation is a different cohort"
    );
    assert!(
        !key.eligible_for(PriceEligibility::AllSubscriptions, None),
        "the successor class is not this line's"
    );
}

// ---------------------------------------------------------------------------
// `inst-plv-lines`' resolution.
// ---------------------------------------------------------------------------

/// Most-specific wins: `(plan, sku)` over `(plan)` over the default line.
#[test]
fn resolution_picks_the_most_specific_line_for_the_priced_row() {
    let sku = TargetSku::new("sku-a").expect("a non-blank sku");
    let lines = vec![
        line(LineKey::list_default(), 500),
        line(LineKey::for_plan(plan(1)), 1000),
        line(LineKey::for_sku(plan(1), sku.clone()), 1500),
    ];

    let resolved = resolve_line(
        &lines,
        plan(1),
        Some(&sku),
        PriceEligibility::AllSubscriptions,
        None,
    )
    .expect("a line applies");
    assert_eq!(
        resolved.adjustment,
        Adjustment::Discount(Magnitude::PercentBp(1500))
    );

    // Same list, a SKU the list does not name: the `(plan)` line wins.
    let other = TargetSku::new("sku-b").expect("a non-blank sku");
    let resolved = resolve_line(
        &lines,
        plan(1),
        Some(&other),
        PriceEligibility::AllSubscriptions,
        None,
    )
    .expect("a line applies");
    assert_eq!(
        resolved.adjustment,
        Adjustment::Discount(Magnitude::PercentBp(1000))
    );

    // A plan the list does not name at all: the default line.
    let resolved = resolve_line(
        &lines,
        plan(9),
        None,
        PriceEligibility::AllSubscriptions,
        None,
    )
    .expect("the default line applies to every target");
    assert_eq!(
        resolved.adjustment,
        Adjustment::Discount(Magnitude::PercentBp(500))
    );
}

/// **The filter runs before the ranking.** A grandfathered row resolves against
/// the cohort line only — the more specific `(plan, sku)` line is not a
/// candidate at all, because it is not eligible.
///
/// This is the case that distinguishes "filter" from "level": under a
/// specificity reading the `(plan, sku)` line would outrank the cohort line and
/// the generation would be repriced.
#[test]
fn the_eligibility_filter_runs_before_the_specificity_ranking() {
    let sku = TargetSku::new("sku-a").expect("a non-blank sku");
    let cohort_key = LineKey::for_plan(plan(1))
        .for_cohort(at(2099))
        .expect("plan is named");
    let lines = vec![
        line(LineKey::for_sku(plan(1), sku.clone()), 1500),
        line(cohort_key, 200),
    ];

    let resolved = resolve_line(
        &lines,
        plan(1),
        Some(&sku),
        PriceEligibility::ExistingGrandfathered,
        Some(at(2099)),
    )
    .expect("the cohort line applies");
    assert_eq!(
        resolved.adjustment,
        Adjustment::Discount(Magnitude::PercentBp(200)),
        "the less specific cohort line wins because the specific one is not eligible"
    );

    // ...and the successor class resolves against the specific line, untouched.
    let resolved = resolve_line(
        &lines,
        plan(1),
        Some(&sku),
        PriceEligibility::AllSubscriptions,
        None,
    )
    .expect("the sku line applies");
    assert_eq!(
        resolved.adjustment,
        Adjustment::Discount(Magnitude::PercentBp(1500))
    );
}

/// A grandfathered row with no matching cohort line resolves to **nothing** —
/// the generation keeps its own immutable price and the stack is empty.
#[test]
fn a_grandfathered_row_with_no_cohort_line_resolves_to_nothing() {
    let lines = vec![
        line(LineKey::list_default(), 500),
        line(LineKey::for_plan(plan(1)), 1000),
    ];
    assert!(
        resolve_line(
            &lines,
            plan(1),
            None,
            PriceEligibility::ExistingGrandfathered,
            Some(at(2099))
        )
        .is_none()
    );
}

// ---------------------------------------------------------------------------
// The magnitude shapes.
// ---------------------------------------------------------------------------

/// `fixed` reports `amount` as its magnitude kind, and there is no other
/// possibility — the type carries an `AmountSet` and not a `Magnitude`.
#[test]
fn a_fixed_line_is_always_amount_based_and_always_replaces() {
    let fixed = Adjustment::Fixed(AmountSet::new([(
        CurrencyCode::new("EUR").expect("a valid code"),
        5000,
    )]));
    assert_eq!(fixed.kind().as_str(), "fixed");
    assert_eq!(fixed.magnitude_kind().as_str(), "amount");
    assert_eq!(fixed.percent_bp(), None);
    assert!(fixed.is_amount_based());
    assert!(
        fixed.replaces(),
        "D-138: `fixed` replaces the running amount"
    );

    let markup = Adjustment::Markup(Magnitude::PercentBp(2000));
    assert_eq!(markup.magnitude_kind().as_str(), "percent_bp");
    assert_eq!(markup.percent_bp(), Some(2000));
    assert!(!markup.is_amount_based());
    assert!(
        !markup.replaces(),
        "only `fixed` voids the layers beneath it"
    );
}

/// An amount-based `markup` is money too — D-08's point, and the reason an
/// additive `fixed` would have been a duplicate of it.
#[test]
fn an_amount_based_markup_carries_per_currency_money() {
    let eur = CurrencyCode::new("EUR").expect("a valid code");
    let usd = CurrencyCode::new("USD").expect("a valid code");
    let markup = Adjustment::Markup(Magnitude::Amount(AmountSet::new([
        (eur.clone(), 500),
        (usd.clone(), 600),
    ])));
    assert_eq!(markup.magnitude_kind().as_str(), "amount");
    let amounts = markup.amounts().expect("an amount line");
    assert_eq!(amounts.get(&eur), Some(500));
    assert_eq!(amounts.get(&usd), Some(600));
    assert_eq!(
        amounts.get(&CurrencyCode::new("GBP").expect("a valid code")),
        None,
        "an uncovered market is what `ADJUSTMENT_CURRENCY_NOT_COVERED` is about"
    );
}

// ---------------------------------------------------------------------------
// The overlay's own interval.
// ---------------------------------------------------------------------------

/// Half-open on both sides, so adjacency is not overlap.
#[test]
fn adjacent_intervals_do_not_intersect() {
    let first = OverlayInterval {
        from: Some(at(2099)),
        to: Some(at(2100)),
    };
    let second = OverlayInterval {
        from: Some(at(2100)),
        to: Some(at(2101)),
    };
    assert!(!first.intersects(&second));
    assert!(!second.intersects(&first));

    let straddling = OverlayInterval {
        from: Some(at(2099)),
        to: Some(at(2101)),
    };
    assert!(straddling.intersects(&second));
}

/// An open-ended interval intersects everything that is not strictly before it.
#[test]
fn an_open_ended_interval_swallows_every_later_one() {
    let open = OverlayInterval {
        from: Some(at(2099)),
        to: None,
    };
    let later = OverlayInterval {
        from: Some(at(2200)),
        to: Some(at(2201)),
    };
    assert!(open.intersects(&later));

    let earlier = OverlayInterval {
        from: None,
        to: Some(at(2099)),
    };
    assert!(
        !earlier.intersects(&open),
        "an interval ending exactly where another starts is adjacent"
    );
}

/// The unbounded interval — no `from`, no `to` — intersects everything, which is
/// what an undated overlay means.
#[test]
fn the_unbounded_interval_intersects_everything() {
    let unbounded = OverlayInterval::default();
    let bounded = OverlayInterval {
        from: Some(at(2099)),
        to: Some(at(2100)),
    };
    assert!(unbounded.intersects(&bounded));
    assert!(bounded.intersects(&unbounded));
}
