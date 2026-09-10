//! Unit tests for `PriceOverlayValidator` — ten of §3's eleven `inst-plv-*`
//! rules, `inst-plv-member-preview` being the one outside this crate. (This line
//! said "the nine" and inherited the module's own miscount; see
//! `overlay_rules.rs`' opening for which id it dropped.)
//!
//! Every case here drives the **pure** rules over a hand-built world, which is
//! deliberate on two counts. Several of these rules have a second guard in the
//! store — the line key's null-safe index, D-67's two range `CHECK`s — and two
//! guards in series are each invisible while the other stands, so the only way
//! to see this one is to call it directly. And the publish-time arm of a rule
//! whose store `CHECK` already refuses the row is unreachable through any
//! repository, which is exactly the shape D-213 recorded one slice over.

use super::*;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::overlay::{AmountSet, Magnitude, ScopeClass, ScopeValue};
use time::OffsetDateTime;

fn plan(n: u128) -> PlanId {
    PlanId::new(Uuid::from_u128(n))
}

fn at(year: i32) -> OffsetDateTime {
    utc_ymd_hms(year, 1, 1, 0, 0, 0)
}

fn eur() -> CurrencyCode {
    CurrencyCode::new("EUR").expect("a valid code")
}

fn usd() -> CurrencyCode {
    CurrencyCode::new("USD").expect("a valid code")
}

fn sku(raw: &str) -> TargetSku {
    TargetSku::new(raw).expect("a non-blank sku")
}

fn brand_scope() -> ScopeSelector {
    ScopeSelector::scoped(
        ScopeClass::Brand,
        ScopeValue::new("acme").expect("a non-blank value"),
    )
    .expect("brand is not global")
}

fn discount_line(key: LineKey, bp: i64) -> OverlayLine {
    OverlayLine {
        line_id: Uuid::from_u128(0x1000 + u128::from(bp.unsigned_abs())),
        key,
        adjustment: Adjustment::Discount(Magnitude::PercentBp(bp)),
    }
}

/// The world in which everything the candidate names is fine: the scope value is
/// declared, plan 1 is published and sells EUR, nothing is retired, no other
/// overlay holds the precedence or an overlapping interval.
fn ordinary_world() -> OverlayWorld {
    OverlayWorld {
        scope_value_declared: true,
        published_plans: [plan(1)].into_iter().collect(),
        published_skus: [(plan(1), [sku("sku-a")].into_iter().collect())]
            .into_iter()
            .collect(),
        retired_plans: BTreeSet::new(),
        sold_currencies: [(plan(1), [eur()].into_iter().collect())]
            .into_iter()
            .collect(),
        published_cohorts: BTreeMap::new(),
        precedence_holder: None,
        interval_holders: Vec::new(),
        layers_beneath: BTreeSet::new(),
        cross_class_ties: Vec::new(),
    }
}

/// The ordinary candidate: a `brand` overlay over plan 1 with one per-plan line.
fn ordinary_candidate() -> OverlayCandidate {
    OverlayCandidate {
        price_overlay_id: Uuid::from_u128(0xA1),
        revision: 0,
        scope: brand_scope(),
        precedence: 10,
        interval: OverlayInterval::default(),
        tax_basis: TaxBasis::DelegatedTariffs,
        disclosure: Disclosure::Restricted,
        target_ref: TargetRef {
            plans: vec![plan(1)],
        },
        lines: vec![discount_line(LineKey::for_plan(plan(1)), 1000)],
        world: ordinary_world(),
    }
}

/// Every code a report raised, in order — what the assertions below compare.
fn codes(report: &ValidationReport) -> Vec<&str> {
    report.violations.iter().map(|v| v.code.as_str()).collect()
}

fn warnings(report: &ValidationReport) -> Vec<&str> {
    report.warnings.iter().map(|w| w.code.as_str()).collect()
}

/// The same, for a **warning** — they live in their own collection, which is why
/// `detail_for` cannot serve both and a test reaching for the wrong one gets `""`
/// rather than a failure that says so.
fn warning_detail_for<'a>(report: &'a ValidationReport, code: &str) -> &'a str {
    report
        .warnings
        .iter()
        .find(|w| w.code == code)
        .map_or("", |w| w.detail.as_str())
}

/// The report names the code **and** says which line, which is what §5 requires
/// of five of these codes ("naming the line").
fn detail_for<'a>(report: &'a ValidationReport, code: &str) -> &'a str {
    report
        .violations
        .iter()
        .find(|v| v.code == code)
        .map_or("", |v| v.detail.as_str())
}

// ---------------------------------------------------------------------------
// The baseline: a candidate with nothing wrong publishes.
// ---------------------------------------------------------------------------

/// An ordinary overlay raises nothing at all.
///
/// Worth its own case: every assertion below is "this candidate raises exactly
/// one code", and that is only informative if the candidate it is derived from
/// raises none.
#[test]
fn an_ordinary_overlay_raises_nothing() {
    let report = validate(&ordinary_candidate());
    assert!(
        report.is_publishable(),
        "the baseline candidate must be clean, got {:?}",
        codes(&report)
    );
    assert!(warnings(&report).is_empty());
}

// ---------------------------------------------------------------------------
// `inst-plv-scope` (D-120).
// ---------------------------------------------------------------------------

/// A scope value no taxonomy declares fails.
///
/// This is the rule the four taxonomy tables were built for: before D-120 the
/// `partner` and `orgTier` classes had no value universe anywhere, so the axis
/// selecting *who receives an adjustment* was a free-form string.
#[test]
fn a_scope_value_outside_its_taxonomy_fails() {
    let mut candidate = ordinary_candidate();
    candidate.world.scope_value_declared = false;

    let report = validate(&candidate);
    assert_eq!(codes(&report), vec![SCOPE_VALUE_UNKNOWN]);
    assert!(
        detail_for(&report, SCOPE_VALUE_UNKNOWN).contains("pricing_brand_taxonomy"),
        "the refusal must name the universe it consulted, got: {}",
        detail_for(&report, SCOPE_VALUE_UNKNOWN)
    );
}

/// The classless overlay validates against **no** universe, because it has no
/// value — so an undeclared-value world cannot make it fail.
#[test]
fn the_classless_overlay_consults_no_taxonomy() {
    let mut candidate = ordinary_candidate();
    candidate.scope = ScopeSelector::Global;
    candidate.world.scope_value_declared = false;

    assert!(validate(&candidate).is_publishable());
}

// ---------------------------------------------------------------------------
// `inst-plv-lines` (D-42).
// ---------------------------------------------------------------------------

/// A zero-line overlay fails — §9's own acceptance criterion.
#[test]
fn an_overlay_with_no_line_fails() {
    let mut candidate = ordinary_candidate();
    candidate.lines.clear();

    let report = validate(&candidate);
    assert_eq!(codes(&report), vec![OVERLAY_HAS_NO_LINE]);
}

/// A duplicate line key fails, and the refusal names the key rather than only
/// the code — a code-only assertion identifies no guard on this surface.
#[test]
fn a_duplicate_line_key_fails() {
    let mut candidate = ordinary_candidate();
    candidate
        .lines
        .push(discount_line(LineKey::for_plan(plan(1)), 2000));

    let report = validate(&candidate);
    assert_eq!(codes(&report), vec![OVERLAY_LINE_DUPLICATE]);
    assert!(
        detail_for(&report, OVERLAY_LINE_DUPLICATE).contains(&plan(1).to_string()),
        "the refusal must name the key that is doubled"
    );
}

/// **The null-safe half, in the domain.** Two list-default lines are a duplicate
/// key, and it is the case the store's naive index would have admitted.
#[test]
fn two_list_default_lines_are_a_duplicate_key() {
    let mut candidate = ordinary_candidate();
    candidate.lines = vec![
        discount_line(LineKey::list_default(), 500),
        discount_line(LineKey::list_default(), 900),
    ];

    assert_eq!(codes(&validate(&candidate)), vec![OVERLAY_LINE_DUPLICATE]);
}

/// A cohort-targeted line and a cohort-less line on one plan are **not** a
/// duplicate: they are disjoint by eligibility (D-78).
#[test]
fn a_cohort_line_does_not_duplicate_its_cohort_less_sibling() {
    let mut candidate = ordinary_candidate();
    candidate.world.published_cohorts = [(plan(1), [at(2099)].into_iter().collect())]
        .into_iter()
        .collect();
    candidate.lines.push(discount_line(
        LineKey::for_plan(plan(1))
            .for_cohort(at(2099))
            .expect("plan is named"),
        200,
    ));

    assert!(validate(&candidate).is_publishable());
}

/// A line naming a plan outside the overlay's `target_ref` fails.
#[test]
fn a_line_outside_the_target_ref_fails() {
    let mut candidate = ordinary_candidate();
    candidate
        .lines
        .push(discount_line(LineKey::for_plan(plan(2)), 500));
    candidate.world.published_plans.insert(plan(2));

    let report = validate(&candidate);
    assert_eq!(codes(&report), vec![OVERLAY_LINE_TARGET_UNKNOWN]);
    assert!(detail_for(&report, OVERLAY_LINE_TARGET_UNKNOWN).contains(&plan(2).to_string()));
}

/// A line naming a SKU its plan does not publish fails under the same code.
#[test]
fn a_line_naming_an_unpublished_sku_fails() {
    let mut candidate = ordinary_candidate();
    candidate.lines = vec![discount_line(LineKey::for_sku(plan(1), sku("sku-z")), 500)];

    let report = validate(&candidate);
    assert_eq!(codes(&report), vec![OVERLAY_LINE_TARGET_UNKNOWN]);
    assert!(detail_for(&report, OVERLAY_LINE_TARGET_UNKNOWN).contains("sku-z"));
}

// ---------------------------------------------------------------------------
// `inst-plv-eligibility` (D-78).
// ---------------------------------------------------------------------------

/// A `cohort` no published grandfathered row of the line's plan carries fails.
#[test]
fn a_line_naming_an_unknown_cohort_fails() {
    let mut candidate = ordinary_candidate();
    candidate.lines = vec![discount_line(
        LineKey::for_plan(plan(1))
            .for_cohort(at(2099))
            .expect("plan is named"),
        200,
    )];

    let report = validate(&candidate);
    assert_eq!(codes(&report), vec![OVERLAY_LINE_COHORT_UNKNOWN]);
    assert!(
        detail_for(&report, OVERLAY_LINE_COHORT_UNKNOWN).contains("2099"),
        "the refusal must name the generation it could not find"
    );
}

/// ...and a `cohort` a published generation does carry passes.
#[test]
fn a_line_naming_a_published_cohort_passes() {
    let mut candidate = ordinary_candidate();
    candidate.world.published_cohorts = [(plan(1), [at(2099)].into_iter().collect())]
        .into_iter()
        .collect();
    candidate.lines = vec![discount_line(
        LineKey::for_plan(plan(1))
            .for_cohort(at(2099))
            .expect("plan is named"),
        200,
    )];

    assert!(validate(&candidate).is_publishable());
}

// ---------------------------------------------------------------------------
// `inst-plv-adjustment` (D-08, D-67, D-138).
// ---------------------------------------------------------------------------

/// **D-67's ceiling.** `discount / percent_bp = 15000` — the "150% of list"
/// data-entry inversion — is refused, and the refusal names the line.
///
/// Before D-67 this passed every stated validation: the only checks were
/// duplicate line keys, out-of-scope targets, per-currency coverage and
/// tax-basis declaration.
#[test]
fn a_discount_above_one_hundred_percent_is_out_of_range() {
    let mut candidate = ordinary_candidate();
    candidate.lines = vec![discount_line(LineKey::for_plan(plan(1)), 15_000)];

    let report = validate(&candidate);
    assert_eq!(codes(&report), vec![ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE]);
    assert!(
        detail_for(&report, ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE).contains("15000"),
        "the refusal must name the value the author typed"
    );
}

/// Exactly 100% is the boundary and is authorable: `0 < v <= 10000`.
#[test]
fn a_discount_of_exactly_one_hundred_percent_is_in_range() {
    let mut candidate = ordinary_candidate();
    candidate.lines = vec![discount_line(LineKey::for_plan(plan(1)), 10_000)];

    assert!(validate(&candidate).is_publishable());
}

/// **D-67's floor**, on both kinds: a non-positive bp magnitude adjusts nothing.
#[test]
fn a_non_positive_bp_magnitude_is_out_of_range() {
    for bp in [0_i64, -500] {
        let mut candidate = ordinary_candidate();
        candidate.lines = vec![discount_line(LineKey::for_plan(plan(1)), bp)];
        assert_eq!(
            codes(&validate(&candidate)),
            vec![ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE],
            "a discount of {bp} bp must be refused"
        );

        let mut candidate = ordinary_candidate();
        candidate.lines = vec![OverlayLine {
            line_id: Uuid::from_u128(0x2000),
            key: LineKey::for_plan(plan(1)),
            adjustment: Adjustment::Markup(Magnitude::PercentBp(bp)),
        }];
        assert_eq!(
            codes(&validate(&candidate)),
            vec![ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE],
            "a markup of {bp} bp must be refused"
        );
    }
}

/// A markup **above** 100% is legal — the ceiling is the discount's alone, and a
/// `+200%` partner markup is a real commercial act.
#[test]
fn a_markup_above_one_hundred_percent_is_in_range() {
    let mut candidate = ordinary_candidate();
    candidate.lines = vec![OverlayLine {
        line_id: Uuid::from_u128(0x2001),
        key: LineKey::for_plan(plan(1)),
        adjustment: Adjustment::Markup(Magnitude::PercentBp(20_000)),
    }];

    assert!(validate(&candidate).is_publishable());
}

/// A negative amount magnitude is refused (D-67), and zero is not — a `fixed 0`
/// line prices a market at nothing, which is a real authoring act.
#[test]
fn a_negative_amount_magnitude_is_out_of_range_and_zero_is_not() {
    let mut candidate = ordinary_candidate();
    candidate.lines = vec![OverlayLine {
        line_id: Uuid::from_u128(0x2002),
        key: LineKey::for_plan(plan(1)),
        adjustment: Adjustment::Fixed(AmountSet::new([(eur(), -1)])),
    }];
    assert_eq!(
        codes(&validate(&candidate)),
        vec![ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE]
    );

    let mut candidate = ordinary_candidate();
    candidate.lines = vec![OverlayLine {
        line_id: Uuid::from_u128(0x2003),
        key: LineKey::for_plan(plan(1)),
        adjustment: Adjustment::Fixed(AmountSet::new([(eur(), 0)])),
    }];
    assert!(validate(&candidate).is_publishable());
}

/// **D-08's coverage rule.** An amount-based line missing a currency its target
/// scope sells fails, naming the line **and** the currency.
#[test]
fn an_amount_line_missing_a_sold_currency_fails() {
    let mut candidate = ordinary_candidate();
    candidate
        .world
        .sold_currencies
        .insert(plan(1), [eur(), usd()].into_iter().collect());
    candidate.lines = vec![OverlayLine {
        line_id: Uuid::from_u128(0x2004),
        key: LineKey::for_plan(plan(1)),
        adjustment: Adjustment::Fixed(AmountSet::new([(eur(), 5000)])),
    }];

    let report = validate(&candidate);
    assert_eq!(codes(&report), vec![ADJUSTMENT_CURRENCY_NOT_COVERED]);
    let detail = detail_for(&report, ADJUSTMENT_CURRENCY_NOT_COVERED);
    assert!(
        detail.contains("USD"),
        "the refusal must name the uncovered market, got: {detail}"
    );
}

/// A **percent** line needs no currency values at all — it is currency-neutral,
/// which is the whole reason the magnitude's type is declared (D-08).
#[test]
fn a_percent_line_needs_no_currency_values() {
    let mut candidate = ordinary_candidate();
    candidate
        .world
        .sold_currencies
        .insert(plan(1), [eur(), usd()].into_iter().collect());

    assert!(validate(&candidate).is_publishable());
}

/// The list-default line's coverage domain is the **union** over `target_ref`,
/// because it applies to every target of the overlay.
#[test]
fn the_list_default_lines_coverage_domain_is_every_target() {
    let mut candidate = ordinary_candidate();
    candidate.target_ref.plans.push(plan(2));
    candidate.world.published_plans.insert(plan(2));
    candidate
        .world
        .sold_currencies
        .insert(plan(2), [usd()].into_iter().collect());
    candidate.lines = vec![OverlayLine {
        line_id: Uuid::from_u128(0x2005),
        key: LineKey::list_default(),
        adjustment: Adjustment::Fixed(AmountSet::new([(eur(), 5000)])),
    }];

    let report = validate(&candidate);
    assert_eq!(codes(&report), vec![ADJUSTMENT_CURRENCY_NOT_COVERED]);
    assert!(detail_for(&report, ADJUSTMENT_CURRENCY_NOT_COVERED).contains("USD"));
}

/// **D-138's warning**, and it is a warning rather than a refusal.
///
/// The predicate reads `layers_beneath` and nothing narrower, and this case
/// cannot say more than that: it sets that set by hand, so it holds whether or
/// not `overlay_facts` puts an equal-precedence lower-class layer in it — which
/// is D-220's first clause. That half is testable only against a repository, and
/// it is
/// `sqlite_overlay_repo::an_equal_precedence_lower_class_overlay_is_beneath_and_ties`.
#[test]
fn a_fixed_line_over_a_lower_layer_warns_and_does_not_block() {
    let mut candidate = ordinary_candidate();
    candidate.world.layers_beneath = [plan(1)].into_iter().collect();
    candidate.lines = vec![OverlayLine {
        line_id: Uuid::from_u128(0x2006),
        key: LineKey::for_plan(plan(1)),
        adjustment: Adjustment::Fixed(AmountSet::new([(eur(), 5000)])),
    }];

    let report = validate(&candidate);
    assert!(
        report.is_publishable(),
        "D-138 warns; it does not refuse — got {:?}",
        codes(&report)
    );
    assert_eq!(warnings(&report), vec![FIXED_LINE_DISCARDS_STACK]);
}

/// **D-230**: a cross-class tie is warned, and it is not a refusal.
///
/// `precedence` is unique only *within* a class, so this pair is legal and the
/// class order breaks it deterministically. The warning exists because an author
/// reading two overlays at "the same precedence" has no reason to expect one to
/// be beneath the other.
#[test]
fn an_equal_precedence_cross_class_pair_warns_and_names_which_is_beneath() {
    let mut candidate = ordinary_candidate();
    let tying = Uuid::from_u128(0x2100);
    candidate.world.cross_class_ties = vec![CrossClassTie {
        price_overlay_id: tying,
        class: ScopeClass::Region,
        plans: [plan(1)].into_iter().collect(),
    }];

    let report = validate(&candidate);

    assert!(
        report.is_publishable(),
        "the tie is legal — got {:?}",
        codes(&report)
    );
    assert_eq!(warnings(&report), vec![EQUAL_PRECEDENCE_CROSS_CLASS_TIE]);
    let detail = warning_detail_for(&report, EQUAL_PRECEDENCE_CROSS_CLASS_TIE);
    assert!(
        detail.contains(&tying.to_string()),
        "names the tying overlay"
    );
    assert!(
        detail.contains("region") && detail.contains("beneath"),
        "and says which side the class order puts beneath: {detail}"
    );
}

/// No tie, no warning — the control without which the case above would also pass
/// against a rule that warned unconditionally.
#[test]
fn no_cross_class_tie_warns_at_all() {
    let candidate = ordinary_candidate();

    assert!(warnings(&validate(&candidate)).is_empty());
}

/// A `fixed` line that **is** the lowest matching layer warns not at all, and a
/// `markup` over a lower layer never warns — only a replacement voids anything.
#[test]
fn only_a_fixed_line_that_is_not_the_lowest_layer_warns() {
    let mut candidate = ordinary_candidate();
    candidate.lines = vec![OverlayLine {
        line_id: Uuid::from_u128(0x2007),
        key: LineKey::for_plan(plan(1)),
        adjustment: Adjustment::Fixed(AmountSet::new([(eur(), 5000)])),
    }];
    assert!(warnings(&validate(&candidate)).is_empty());

    let mut candidate = ordinary_candidate();
    candidate.world.layers_beneath = [plan(1)].into_iter().collect();
    assert!(
        warnings(&validate(&candidate)).is_empty(),
        "a discount does not discard the layers beneath it"
    );
}

// ---------------------------------------------------------------------------
// `inst-plv-precedence` (L2) and `inst-plv-dating` (D-107).
// ---------------------------------------------------------------------------

/// A precedence another **published** overlay of the same class holds fails.
#[test]
fn a_precedence_another_overlay_holds_fails() {
    let mut candidate = ordinary_candidate();
    candidate.world.precedence_holder = Some(Uuid::from_u128(0xB2));

    let report = validate(&candidate);
    assert_eq!(codes(&report), vec![PRECEDENCE_DUPLICATE]);
    assert!(
        detail_for(&report, PRECEDENCE_DUPLICATE).contains(&Uuid::from_u128(0xB2).to_string()),
        "the refusal must name the overlay holding the slot, so the operator can go and edit it"
    );
}

/// **D-107.** An overlapping interval on the candidate's **own** predecessor
/// revision is not a collision — the collision domain is *other* overlays'
/// published revisions.
///
/// Without this the unscoped check matched the overlay's own published revision
/// and rejected every edit of a live overlay.
#[test]
fn an_overlay_does_not_collide_with_its_own_published_revision() {
    let mut candidate = ordinary_candidate();
    candidate.revision = 1;
    candidate.interval = OverlayInterval {
        from: Some(at(2099)),
        to: Some(at(2101)),
    };
    candidate.world.interval_holders = vec![PublishedLineInterval {
        price_overlay_id: candidate.price_overlay_id,
        scope: brand_scope(),
        key: LineKey::for_plan(plan(1)),
        interval: OverlayInterval {
            from: Some(at(2099)),
            to: Some(at(2101)),
        },
    }];

    assert!(
        validate(&candidate).is_publishable(),
        "the collision domain is *other* overlays' published revisions"
    );
}

/// ...while a **different** overlay's overlapping line interval still fails.
#[test]
fn a_different_overlays_overlapping_line_interval_fails() {
    let mut candidate = ordinary_candidate();
    candidate.interval = OverlayInterval {
        from: Some(at(2099)),
        to: Some(at(2101)),
    };
    candidate.world.interval_holders = vec![PublishedLineInterval {
        price_overlay_id: Uuid::from_u128(0xB2),
        scope: brand_scope(),
        key: LineKey::for_plan(plan(1)),
        interval: OverlayInterval {
            from: Some(at(2100)),
            to: None,
        },
    }];

    let report = validate(&candidate);
    assert_eq!(codes(&report), vec![OVERLAY_INTERVAL_OVERLAP]);
    assert!(
        detail_for(&report, OVERLAY_INTERVAL_OVERLAP).contains(&Uuid::from_u128(0xB2).to_string())
    );
}

/// The collision key carries the **scope**, so one plan's line under two
/// different brands never collides.
#[test]
fn two_scopes_never_collide_on_one_line_key() {
    let mut candidate = ordinary_candidate();
    candidate.interval = OverlayInterval {
        from: Some(at(2099)),
        to: Some(at(2101)),
    };
    candidate.world.interval_holders = vec![PublishedLineInterval {
        price_overlay_id: Uuid::from_u128(0xB2),
        scope: ScopeSelector::scoped(
            ScopeClass::Brand,
            ScopeValue::new("other").expect("a non-blank value"),
        )
        .expect("brand is not global"),
        key: LineKey::for_plan(plan(1)),
        interval: OverlayInterval {
            from: Some(at(2100)),
            to: None,
        },
    }];

    assert!(validate(&candidate).is_publishable());
}

/// **D-78 extended the collision key with `cohort`**: a cohort-targeted line and
/// a cohort-less line on one `(plan, sku)` are disjoint by eligibility, so they
/// never collide — matching the within-overlay `UNIQUE`.
#[test]
fn a_cohort_line_never_collides_with_a_cohort_less_one() {
    let mut candidate = ordinary_candidate();
    candidate.world.published_cohorts = [(plan(1), [at(2099)].into_iter().collect())]
        .into_iter()
        .collect();
    candidate.lines = vec![discount_line(
        LineKey::for_plan(plan(1))
            .for_cohort(at(2099))
            .expect("plan is named"),
        200,
    )];
    candidate.interval = OverlayInterval {
        from: Some(at(2099)),
        to: Some(at(2101)),
    };
    candidate.world.interval_holders = vec![PublishedLineInterval {
        price_overlay_id: Uuid::from_u128(0xB2),
        scope: brand_scope(),
        key: LineKey::for_plan(plan(1)),
        interval: OverlayInterval {
            from: Some(at(2100)),
            to: None,
        },
    }];

    assert!(validate(&candidate).is_publishable());
}

// ---------------------------------------------------------------------------
// `inst-plv-referential` (D-31).
// ---------------------------------------------------------------------------

/// An overlay targeting an unpublished plan is rejected.
#[test]
fn an_unpublished_target_fails() {
    let mut candidate = ordinary_candidate();
    candidate.world.published_plans.clear();

    let report = validate(&candidate);
    assert!(
        codes(&report).contains(&TARGET_UNPUBLISHED),
        "got {:?}",
        codes(&report)
    );
    assert!(detail_for(&report, TARGET_UNPUBLISHED).contains(&plan(1).to_string()));
}

/// **D-31: a retired target dangles and is flagged, never blocked.**
///
/// In-flight subscribers legitimately keep resolving a retired plan's rows, so
/// the overlay stays evaluable for them; remediation is to end or retarget it.
/// A retire-time block would be exactly the rule D-31 refused — and it would
/// block the remediation too, since ending or retargeting an overlay is itself a
/// submit.
///
/// # The world here is the one the schema can actually hold
///
/// A retired plan is **still in `published_plans`**, and that is not a
/// convenience of this fixture: `uq_pricing_plan_current` is
/// `UNIQUE (plan_id) WHERE lifecycle_state IN ('published','retired')`, so
/// retirement flips the plan's one **current** revision in place and `published`
/// and `retired` are two spellings of current (D-128).
///
/// An earlier version of this case set `retired_plans` **without**
/// `published_plans` — a state the schema cannot produce — and it passed while
/// `plan_facts` put a retired plan in neither set, so every overlay on a retired
/// plan was refused `TARGET_UNPUBLISHED`. A fixture asserting about a world the
/// system would not hold is a fixture that proves nothing.
#[test]
fn a_retired_target_warns_and_does_not_block() {
    let mut candidate = ordinary_candidate();
    // Retired **and** current: what the plan table actually holds.
    candidate.world.retired_plans = [plan(1)].into_iter().collect();
    assert!(
        candidate.world.published_plans.contains(&plan(1)),
        "a retired plan is still the plan's current revision (D-128)"
    );

    let report = validate(&candidate);
    assert!(
        report.is_publishable(),
        "D-31 is dangling-and-flagged, not blocked — got {:?}",
        codes(&report)
    );
    assert_eq!(warnings(&report), vec![TARGET_RETIRED]);
}

/// §1.7's *"effective-interval sanity"*, at the authoring edge.
///
/// An inverted or empty interval is refused before the store sees it. It has to
/// be its own entry point rather than an arm of [`validate`]: an inverted
/// interval intersects **nothing** (`OverlayInterval::intersects` fails its first
/// conjunct), so `check_dating` would silently find no collision for it rather
/// than reporting one — the sanity check is what makes the collision walk mean
/// anything.
#[test]
fn an_inverted_or_empty_interval_is_refused_at_the_authoring_edge() {
    let lines = vec![discount_line(LineKey::for_plan(plan(1)), 1000)];

    for (from, to) in [(at(2101), at(2099)), (at(2099), at(2099))] {
        let report = check_authored_shape(
            OverlayInterval {
                from: Some(from),
                to: Some(to),
            },
            &lines,
        );
        assert_eq!(
            codes(&report),
            vec![OVERLAY_INTERVAL_INVALID],
            "[{from}, {to}) is empty and must be refused"
        );
    }

    // Both open arms are legal, and so is an ordinary interval.
    for interval in [
        OverlayInterval::default(),
        OverlayInterval {
            from: Some(at(2099)),
            to: None,
        },
        OverlayInterval {
            from: None,
            to: Some(at(2099)),
        },
        OverlayInterval {
            from: Some(at(2099)),
            to: Some(at(2101)),
        },
    ] {
        assert!(check_authored_shape(interval, &lines).is_publishable());
    }
}

/// **`OVERLAY_LINE_DUPLICATE` is reachable at the authoring edge**, which is the
/// only place it can fire.
///
/// The store's null-safe index refuses a duplicate on the INSERT, so before this
/// entry point existed a duplicate answered **500** — and, because the save never
/// landed, the `check_lines` arm that raises the same code at submit had no
/// reachable path either. A code §5 declares with no producer.
#[test]
fn a_duplicate_line_key_is_refused_at_the_authoring_edge() {
    let report = check_authored_shape(
        OverlayInterval::default(),
        &[
            discount_line(LineKey::list_default(), 500),
            discount_line(LineKey::list_default(), 900),
        ],
    );
    assert_eq!(codes(&report), vec![OVERLAY_LINE_DUPLICATE]);
}

// ---------------------------------------------------------------------------
// `inst-plv-taxbasis` (L5) — the entry point of its own.
// ---------------------------------------------------------------------------

/// *Silence fails.* An absent basis is `TAX_BASIS_UNDECLARED`, and an explicit
/// delegation to Tariffs is a **declaration** rather than a silence.
#[test]
fn an_absent_tax_basis_fails_and_an_explicit_delegation_does_not() {
    assert_eq!(check_tax_basis_declared(None), Err(TAX_BASIS_UNDECLARED));
    assert_eq!(
        check_tax_basis_declared(Some(TaxBasis::DelegatedTariffs)),
        Ok(TaxBasis::DelegatedTariffs)
    );
    assert_eq!(
        check_tax_basis_declared(Some(TaxBasis::Inclusive)),
        Ok(TaxBasis::Inclusive)
    );
}

// ---------------------------------------------------------------------------
// The report is aggregate.
// ---------------------------------------------------------------------------

/// Every fault is reported in one pass, which is what makes an overlay
/// remediable in one edit rather than in as many edits as it has faults
/// (Foundation §4.2).
#[test]
fn every_fault_is_reported_in_one_pass() {
    let mut candidate = ordinary_candidate();
    candidate.world.scope_value_declared = false;
    candidate.world.precedence_holder = Some(Uuid::from_u128(0xB2));
    candidate.lines = vec![
        discount_line(LineKey::for_plan(plan(1)), 15_000),
        discount_line(LineKey::for_plan(plan(1)), 900),
    ];

    let report = validate(&candidate);
    let raised = codes(&report);
    for expected in [
        SCOPE_VALUE_UNKNOWN,
        OVERLAY_LINE_DUPLICATE,
        ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE,
        PRECEDENCE_DUPLICATE,
    ] {
        assert!(
            raised.contains(&expected),
            "{expected} must be in the one-pass report, got {raised:?}"
        );
    }
}
