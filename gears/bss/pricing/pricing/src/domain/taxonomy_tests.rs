//! `TaxonomyValidator`'s two owned rules, and the vocabulary the four taxonomies
//! are addressed by (`design/04-currency-tax.md` §3, §5, §6).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{
    GL_CODE_UNKNOWN, GlCodeDeclared, REGION_UNKNOWN, RegionTaxMarkers, RegionsDeclared,
    TAXONOMY_VALUE_IN_USE, TaxCategoryPatch, TaxonomyClass, TaxonomyEntry, TaxonomyState,
    TaxonomyValuePatch, ValueReferences, check_retirable, check_tax_category_removable,
    edit_is_governed, tag_of, tag_of_value,
};
use crate::domain::concurrency::RowVersion;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::{CurrencyCode, MinorAmount};
use crate::domain::overlay::{ScopeClass, ScopeValue};
use crate::domain::plan_shape::PlanShape;
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::PriceRow;
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::domain::validation::{ValidationReport, ValidationRule};
use std::collections::BTreeSet;
use time::OffsetDateTime;

fn now() -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 6, 12, 0, 0)
}

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x91a4))
}

fn value(raw: &str) -> ScopeValue {
    ScopeValue::new(raw).expect("a non-blank fixture value")
}

fn region(raw: &str) -> Region {
    Region::new(raw).expect("a non-blank fixture region")
}

/// One candidate row on a named region.
fn row_in(price_id: u128, in_region: &str) -> PriceRecord {
    let scope_key = ScopeKey::new(
        plan(),
        CurrencyCode::new("EUR").expect("three letters"),
        region(in_region),
        PhaseId::new(Uuid::from_u128(0xf1)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none");

    let mut row = PriceRow::new(ChargeKind::Recurring, None);
    row.amount_minor = Some(MinorAmount::new(1000).expect("non-negative"));

    PriceRecord {
        price_id: Uuid::from_u128(price_id),
        scope_key,
        row,
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

/// A plan shape carrying exactly the named regions, one row each.
fn shape_over(regions: &[&str]) -> PlanShape {
    let mut shape = PlanShape::new(plan(), 1, now());
    shape.rows = regions
        .iter()
        .enumerate()
        .map(|(n, r)| row_in(0xb000 + n as u128, r))
        .collect();
    shape
}

fn declared(regions: &[&str]) -> RegionsDeclared {
    RegionsDeclared {
        declared: regions.iter().map(|r| region(r)).collect(),
    }
}

fn codes(report: &ValidationReport) -> Vec<String> {
    report.violations.iter().map(|v| v.code.clone()).collect()
}

fn subjects(report: &ValidationReport) -> Vec<String> {
    report
        .violations
        .iter()
        .map(|v| v.subject.clone())
        .collect()
}

fn run(rule: &RegionsDeclared, shape: &PlanShape) -> ValidationReport {
    let mut report = ValidationReport::default();
    rule.evaluate(shape, &mut report);
    report
}

// ---------------------------------------------------------------------------
// The codes, transcribed from section 5 rather than from the identifiers.
// ---------------------------------------------------------------------------

/// A constant whose value drifted from its name would still compile and would
/// still be wrong on the wire.
#[test]
fn the_codes_are_spelled_as_section_5_spells_them() {
    assert_eq!(REGION_UNKNOWN, "REGION_UNKNOWN");
    assert_eq!(TAXONOMY_VALUE_IN_USE, "TAXONOMY_VALUE_IN_USE");
    assert_eq!(GL_CODE_UNKNOWN, "GL_CODE_UNKNOWN");
}

// ---------------------------------------------------------------------------
// The classes.
// ---------------------------------------------------------------------------

/// Every class is addressed by **the same token the overlay plane stores for it**
/// (D-241).
///
/// The segment used to be §5's literal spelling, and `orgTier` was a recorded
/// divergence pinned here on purpose (`T-4`) so that changing it would be a
/// decision rather than a tidy-up. The owner made it: an `OpenAPI`-generated client
/// carrying two spellings for one class is the cost that decides, and this gear is
/// not deployed, so nothing breaks.
///
/// **The rule is enforced structurally rather than asserted here**, and this test
/// says so instead of pretending otherwise: `path_segment` now *returns*
/// `scope_class().as_str()`, so `segment == scope token` cannot fail and asserting
/// it would be decoration. What is left is the part a delegation cannot carry —
/// **the four strings that actually go on the wire**, which pin `ScopeClass`'
/// spelling itself and would redden if that drifted under either surface.
#[test]
fn a_class_is_addressed_by_the_token_the_overlay_plane_stores_for_it() {
    let segments: Vec<&str> = TaxonomyClass::ALL
        .iter()
        .map(|c| c.path_segment())
        .collect();
    assert_eq!(segments, ["region", "brand", "partner", "org_tier"]);
}

/// Every segment parses back to the class that rendered it, and nothing else
/// parses at all.
#[test]
fn a_segment_round_trips_and_an_unknown_one_does_not_parse() {
    for &class in TaxonomyClass::ALL {
        assert_eq!(
            TaxonomyClass::parse_segment(class.path_segment()),
            Some(class),
            "{class} must parse back from its own segment"
        );
    }
    // The two classes that are deliberately not addressable: `global` has no
    // value universe, and `customerGroup`'s table belongs to Slice 9's
    // membership half and does not exist (D-223).
    assert_eq!(TaxonomyClass::parse_segment("global"), None);
    assert_eq!(TaxonomyClass::parse_segment("customer_group"), None);
    // And the camelCase spelling §5 used to carry, which D-241 retired: it is not
    // an alias, because two spellings that both route is the state in which
    // neither is canonical.
    assert_eq!(TaxonomyClass::parse_segment("orgTier"), None);
}

/// A region's `taxCategory` cannot be dropped while a published row resolves
/// through it (D-245).
///
/// The count is in the message because the operator's next move depends on it:
/// one row is an edit, forty is a decision about the default. And the message
/// names the grandfathered case explicitly, because that is the one where "author
/// the category onto those rows" is **not** available — an `existing_grandfathered`
/// row MUST NOT be superseded and cannot be `PATCH`ed, which is the whole reason
/// D-245 guards the marker instead of relaxing `TaxBasisComplete`.
#[test]
fn a_tax_category_a_published_row_resolves_through_cannot_be_dropped() {
    let report = check_tax_category_removable(&value("eu"), 2);

    assert_eq!(codes(&report), [TAXONOMY_VALUE_IN_USE]);
    let detail = &report.violations[0].detail;
    assert!(detail.contains('2'), "the count is actionable: {detail}");
    assert!(
        detail.contains("grandfathered"),
        "the case with no remedy on the row side must be named: {detail}"
    );
    assert_eq!(subjects(&report), ["eu"]);
}

/// A marker nothing leans on comes off cleanly.
///
/// The control the case above needs: without it the guard could refuse every
/// removal and still look correct.
#[test]
fn a_tax_category_nothing_resolves_through_comes_off_cleanly() {
    assert!(check_tax_category_removable(&value("eu"), 0).is_publishable());
}

/// The four physical tables this crate queries, pinned as literals.
///
/// **The delegation is not what is asserted here**, in the shape the sibling
/// above already uses for `path_segment`: `TaxonomyClass::table` *returns*
/// `scope_class().taxonomy_table()`, so comparing the two cannot fail and there
/// is no second map to fork. What a delegation cannot carry is the strings
/// themselves, and these four are the ones the crate *renders*:
/// `taxonomy_repo`'s `CorruptRow` diagnostics name the relation a bad row came
/// from, and `overlay_rules`' `SCOPE_VALUE_UNKNOWN` detail names the universe a
/// scope value is missing from. **The store does not read them** — every query
/// goes through a `SeaORM` entity whose `#[sea_orm(table_name = …)]` is its own
/// spelling — so a rename in the schema reddens the goldens rather than this
/// case, and what this case holds is that the map and the messages do not drift
/// apart from it silently. The `expect` keeps the other half: every one of the
/// four narrow classes declares a table at all.
#[test]
fn each_class_names_the_table_the_store_queries() {
    for &class in TaxonomyClass::ALL {
        class
            .scope_class()
            .taxonomy_table()
            .expect("all four narrow classes declare a table");
    }
    let tables: Vec<&str> = TaxonomyClass::ALL.iter().map(|c| c.table()).collect();
    assert_eq!(
        tables,
        [
            "pricing_region_taxonomy",
            "pricing_brand_taxonomy",
            "pricing_partner_taxonomy",
            "pricing_org_tier_taxonomy",
        ]
    );
    assert_eq!(
        TaxonomyClass::Region.scope_class(),
        ScopeClass::Region,
        "the narrowing must not renumber the classes"
    );
    assert_eq!(TaxonomyClass::OrgTier.scope_class(), ScopeClass::OrgTier);
}

/// D-01's markers are the region's alone — the schema says so and so does §6.
#[test]
fn only_the_region_class_carries_the_tax_markers() {
    assert!(TaxonomyClass::Region.carries_tax_markers());
    for &class in TaxonomyClass::ALL
        .iter()
        .filter(|c| **c != TaxonomyClass::Region)
    {
        assert!(
            !class.carries_tax_markers(),
            "{class} must not claim columns the schema does not give it"
        );
    }
}

/// `active | retired`, both directions, and nothing else.
#[test]
fn the_state_pair_round_trips_and_admits_no_third_token() {
    for &state in TaxonomyState::ALL {
        assert_eq!(TaxonomyState::parse(state.as_str()), Some(state));
    }
    assert_eq!(TaxonomyState::parse("deprecated"), None);
    assert_eq!(
        TaxonomyState::default(),
        TaxonomyState::Active,
        "a value an operator declares is declared, not withdrawn"
    );
}

// ---------------------------------------------------------------------------
// `inst-tx-region` — price-row membership.
// ---------------------------------------------------------------------------

#[test]
fn a_row_in_a_declared_region_passes() {
    let report = run(&declared(&["eu", "us"]), &shape_over(&["eu"]));
    assert!(report.is_publishable(), "a declared region blocks nothing");
    assert_eq!(codes(&report), Vec::<String>::new());
}

#[test]
fn a_row_in_an_undeclared_region_fails_naming_the_value() {
    let report = run(&declared(&["eu"]), &shape_over(&["ap-south"]));

    assert_eq!(codes(&report), [REGION_UNKNOWN]);
    assert_eq!(
        subjects(&report),
        ["ap-south"],
        "the refusal names the value the operator must declare"
    );
    let detail = &report.violations[0].detail;
    assert!(
        detail.contains("ap-south"),
        "the detail names the value too: {detail}"
    );
    assert!(
        detail.contains("config/taxonomies/region"),
        "and tells the operator where to declare it: {detail}"
    );
}

/// One violation per undeclared **value**, not per row.
///
/// A plan with three rows in one bad region is one authoring mistake. Three
/// copies of it is a report an operator scrolls past, and it is how an aggregate
/// report stops being readable as the row count grows — the 20-currency floor of
/// C1 makes that a real number, not a hypothetical.
#[test]
fn one_undeclared_region_across_many_rows_is_reported_once() {
    let report = run(
        &declared(&["eu"]),
        &shape_over(&["ap-south", "ap-south", "ap-south"]),
    );

    assert_eq!(codes(&report), [REGION_UNKNOWN]);
}

/// Two different undeclared regions are two different facts.
#[test]
fn two_undeclared_regions_are_reported_separately() {
    let report = run(&declared(&["eu"]), &shape_over(&["ap-south", "sa-east"]));

    assert_eq!(codes(&report), [REGION_UNKNOWN, REGION_UNKNOWN]);
    assert_eq!(subjects(&report), ["ap-south", "sa-east"]);
}

/// A tenant who has declared nothing fails every row, and that is C2 rather than
/// an edge case.
///
/// *"membership is validated at save/publish (unknown value fails before
/// publish)"* carves out no tenant. The opposite reading — an empty universe
/// permits everything — is the shape D-211 was written against, and it is the
/// difference between a fail-closed rule and a rule that is off until someone
/// configures it on.
#[test]
fn a_tenant_with_no_declared_region_fails_closed_rather_than_open() {
    let report = run(&declared(&[]), &shape_over(&["eu"]));

    assert_eq!(codes(&report), [REGION_UNKNOWN]);
}

/// The rule is registered under the instruction id it implements.
#[test]
fn the_rule_names_the_instruction_it_implements() {
    assert_eq!(declared(&[]).name(), "inst-tx-region");
}

// ---------------------------------------------------------------------------
// `inst-tx-mutation` — the retire guard.
// ---------------------------------------------------------------------------

#[test]
fn an_unreferenced_value_retires_cleanly() {
    let report = check_retirable(
        TaxonomyClass::Brand,
        &value("acme"),
        ValueReferences::default(),
    );
    assert!(report.is_publishable());
}

#[test]
fn a_value_a_published_row_names_cannot_retire() {
    let report = check_retirable(
        TaxonomyClass::Region,
        &value("eu"),
        ValueReferences {
            published_price_rows: 4,
            active_overlay_scopes: 0,
        },
    );

    assert_eq!(codes(&report), [TAXONOMY_VALUE_IN_USE]);
    assert_eq!(subjects(&report), ["eu"]);
}

/// D-120's widening, which is the whole reason the guard is written over a class.
///
/// Before it, *"a region value retired cleanly while region-scoped overlays still
/// named it"*. The overlay plane is checked for **every** class, including the
/// three that are not price-row axes at all.
#[test]
fn a_value_only_an_overlay_scope_names_cannot_retire_either_in_any_class() {
    for &class in TaxonomyClass::ALL {
        let report = check_retirable(
            class,
            &value("acme"),
            ValueReferences {
                published_price_rows: 0,
                active_overlay_scopes: 1,
            },
        );
        assert_eq!(
            codes(&report),
            [TAXONOMY_VALUE_IN_USE],
            "{class}: an overlay scope is a reference on every taxonomy-backed class (D-120)"
        );
    }
}

/// The refusal names each plane's count **as rendered**, and names only the
/// planes the class actually has.
///
/// Rewritten after review found the first version vacuous: it asserted
/// `detail.contains('1')`, which the trailing `(D-120)` satisfies for any input,
/// and `detail.contains("region")`, which a hardcoded literal in the template
/// satisfied for every class. Swapping the two format arguments left it green
/// while the operator was told the wrong count on each plane.
#[test]
fn the_refusal_names_each_planes_count_as_rendered() {
    let report = check_retirable(
        TaxonomyClass::Region,
        &value("eu"),
        ValueReferences {
            published_price_rows: 1,
            active_overlay_scopes: 9,
        },
    );

    let detail = &report.violations[0].detail;
    assert!(
        detail.contains("1 published price row(s)"),
        "the row count is rendered on its own plane: {detail}"
    );
    assert!(
        detail.contains("9 published overlay scope(s)"),
        "and the overlay count on its own: {detail}"
    );
    assert!(
        detail.starts_with("region value `eu` cannot retire"),
        "and the class is the subject rather than a word from the template: {detail}"
    );
}

/// A class with no row plane is not told about one.
///
/// `published_price_rows` is a structural zero for `brand`, `partner` and
/// `orgTier` — `references_to` does not run that query for them — so rendering it
/// invited an operator to audit a plane §3 step 2 says their value cannot be on.
#[test]
fn a_non_region_refusal_does_not_mention_a_price_row_plane() {
    let report = check_retirable(
        TaxonomyClass::Partner,
        &value("reseller-a"),
        ValueReferences {
            published_price_rows: 0,
            active_overlay_scopes: 3,
        },
    );

    let detail = &report.violations[0].detail;
    assert!(
        detail.starts_with("partner value `reseller-a` cannot retire"),
        "{detail}"
    );
    assert!(
        !detail.contains("price row"),
        "a partner value is not a price-row axis, so no such plane is named: {detail}"
    );
    assert!(detail.contains("3 published overlay scope(s)"), "{detail}");
}

// ---------------------------------------------------------------------------
// D-355: which edits are governed at all.
// ---------------------------------------------------------------------------

/// The predicate the `PATCH` door reads: the same two planes the retire guard
/// counts decide whether a second principal is owed.
#[test]
fn an_edit_is_governed_iff_something_published_names_the_value() {
    // Nothing published -> the operator's own act.
    assert!(!edit_is_governed(ValueReferences::default()));
    // A published price row on the region axis -> governed.
    assert!(edit_is_governed(ValueReferences {
        published_price_rows: 1,
        active_overlay_scopes: 0,
    }));
    // A published overlay scope -> governed (the plane every class has).
    assert!(edit_is_governed(ValueReferences {
        published_price_rows: 0,
        active_overlay_scopes: 1,
    }));
}

// ---------------------------------------------------------------------------
// D-353: the per-value edit and its tag.
// ---------------------------------------------------------------------------

fn region_entry() -> TaxonomyEntry {
    TaxonomyEntry {
        value: ScopeValue::new("eu").expect("a value"),
        display_name: "EU".to_owned(),
        state: TaxonomyState::Active,
        tax: Some(RegionTaxMarkers {
            tax_category: Some("vat_standard".to_owned()),
            tax_rate_present: true,
        }),
    }
}

#[test]
fn an_empty_patch_applies_to_the_held_value_unchanged() {
    let held = region_entry();
    assert_eq!(TaxonomyValuePatch::default().apply(&held), held);
}

#[test]
fn a_patch_changes_only_what_it_names_and_null_clears_the_category() {
    let held = region_entry();
    let relabelled = TaxonomyValuePatch {
        display_name: Some("Europe".to_owned()),
        ..TaxonomyValuePatch::default()
    }
    .apply(&held);
    assert_eq!(relabelled.display_name, "Europe");
    assert_eq!(relabelled.state, held.state);
    assert_eq!(relabelled.tax, held.tax, "unnamed markers are kept");

    let cleared = TaxonomyValuePatch {
        tax_category: TaxCategoryPatch::Clear,
        ..TaxonomyValuePatch::default()
    }
    .apply(&held);
    assert_eq!(
        cleared.tax.as_ref().and_then(|t| t.tax_category.as_deref()),
        None
    );
    assert!(
        cleared.tax.as_ref().is_some_and(|t| t.tax_rate_present),
        "the other marker stays"
    );

    let retired = TaxonomyValuePatch {
        state: Some(TaxonomyState::Retired),
        ..TaxonomyValuePatch::default()
    }
    .apply(&held);
    assert_eq!(retired.state, TaxonomyState::Retired);
}

#[test]
fn the_markers_in_a_patch_are_inert_on_a_class_without_them() {
    let held = TaxonomyEntry {
        tax: None,
        ..region_entry()
    };
    let patched = TaxonomyValuePatch {
        tax_category: TaxCategoryPatch::Set("x".to_owned()),
        tax_rate_present: Some(true),
        ..TaxonomyValuePatch::default()
    }
    .apply(&held);
    assert_eq!(
        patched.tax, None,
        "the route refuses them earlier; the domain never invents a column"
    );
}

#[test]
fn a_values_tag_moves_with_each_rendered_field_and_is_not_the_sets() {
    let held = region_entry();
    let base = tag_of_value(TaxonomyClass::Region, &held);
    assert_ne!(
        base,
        tag_of(TaxonomyClass::Region, std::slice::from_ref(&held)),
        "one value is not the set"
    );
    assert_ne!(
        base,
        tag_of_value(TaxonomyClass::Brand, &held),
        "one value in two classes is two resources"
    );
    for patch in [
        TaxonomyValuePatch {
            display_name: Some("Europe".to_owned()),
            ..TaxonomyValuePatch::default()
        },
        TaxonomyValuePatch {
            state: Some(TaxonomyState::Retired),
            ..TaxonomyValuePatch::default()
        },
        TaxonomyValuePatch {
            tax_category: TaxCategoryPatch::Clear,
            ..TaxonomyValuePatch::default()
        },
        TaxonomyValuePatch {
            tax_rate_present: Some(false),
            ..TaxonomyValuePatch::default()
        },
    ] {
        assert_ne!(
            base,
            tag_of_value(TaxonomyClass::Region, &patch.apply(&held)),
            "{patch:?}"
        );
    }
    assert_eq!(
        base,
        tag_of_value(TaxonomyClass::Region, &held),
        "an unchanged value renders one tag"
    );
}

// ---------------------------------------------------------------------------
// `GlCodeDeclared` — the descriptor's one GL code against the declared set (D-356).
// ---------------------------------------------------------------------------

fn gl_codes(values: &[&str]) -> GlCodeDeclared {
    GlCodeDeclared {
        declared: values.iter().map(|v| (*v).to_owned()).collect(),
    }
}

/// A plan whose descriptor set carries `gl_code`, and nothing else the rule
/// reads.
fn plan_with_gl_code(gl_code: Option<&str>) -> PlanShape {
    use crate::domain::plan_shape::DescriptorSet;
    let mut shape = PlanShape::new(plan(), 1, now());
    shape.descriptor_set = Some(DescriptorSet {
        invoice_line_template: Some("{plan}".to_owned()),
        gl_code: gl_code.map(ToOwned::to_owned),
        itemization_rule: Some("per_charge".to_owned()),
        additional: std::collections::BTreeMap::new(),
    });
    shape
}

fn run_gl(rule: &GlCodeDeclared, shape: &PlanShape) -> ValidationReport {
    let mut report = ValidationReport::default();
    rule.evaluate(shape, &mut report);
    report
}

/// Membership is checked **only once a vocabulary is declared** — the empty set
/// is the opt-out state, `RoundingPolicyDeclared`'s asymmetry and the invariant
/// that let the table land without failing every existing plan.
#[test]
fn gl_code_membership_is_checked_only_once_a_vocabulary_is_declared() {
    let unconstrained = GlCodeDeclared {
        declared: BTreeSet::new(),
    };
    assert!(unconstrained.violation_for("glCode", "4000-REV").is_none());
    assert!(
        unconstrained
            .violation_for("glCode", "anything at all")
            .is_none()
    );

    let declared = gl_codes(&["4000-REV", "4010-TAX"]);
    assert!(declared.violation_for("glCode", "4000-REV").is_none());
    let v = declared
        .violation_for("glCode", "9999-BAD")
        .expect("an unknown code is refused");
    assert_eq!(v.code, GL_CODE_UNKNOWN);
    assert_eq!(v.subject, "glCode");
    assert!(
        v.detail.contains("9999-BAD") && v.detail.contains("/config/gl-codes"),
        "the finding names the code and the door that declares it: {}",
        v.detail
    );
}

/// The rule reads the descriptor's **one** value and reports at most once — not a
/// row walk — and it judges a present value only: an absent or blank `glCode` is
/// `inst-ds-required`'s finding, and a second report of the same absence would
/// send the author to a vocabulary for a field they have not filled in.
#[test]
fn gl_code_rule_judges_the_descriptors_one_present_value_and_nothing_else() {
    let rule = gl_codes(&["4000-REV"]);

    assert_eq!(rule.name(), "inst-ds-glcode");

    let refused = run_gl(&rule, &plan_with_gl_code(Some("4000")));
    assert_eq!(codes(&refused), [GL_CODE_UNKNOWN]);
    assert_eq!(
        subjects(&refused),
        [plan_with_gl_code(Some("4000")).subject()],
        "the subject is the plan, as `inst-ds-required`'s is"
    );

    assert!(codes(&run_gl(&rule, &plan_with_gl_code(Some("4000-REV")))).is_empty());
    assert!(
        codes(&run_gl(&rule, &plan_with_gl_code(None))).is_empty(),
        "absence is `inst-ds-required`'s finding"
    );
    assert!(
        codes(&run_gl(&rule, &plan_with_gl_code(Some("   ")))).is_empty(),
        "a blank is an absence wearing a value's shape, and is likewise not this rule's"
    );
    let mut without_set = plan_with_gl_code(Some("4000"));
    without_set.descriptor_set = None;
    assert!(codes(&run_gl(&rule, &without_set)).is_empty());

    assert!(
        codes(&run_gl(
            &GlCodeDeclared {
                declared: BTreeSet::new()
            },
            &plan_with_gl_code(Some("4000"))
        ))
        .is_empty(),
        "and the empty set constrains nothing at the plan level too"
    );
}
