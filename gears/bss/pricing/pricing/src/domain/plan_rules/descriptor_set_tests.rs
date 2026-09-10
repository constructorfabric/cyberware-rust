//! Tests for `inst-ds-required`.
//!
//! The rule has one job with two halves: report **every** missing element, and
//! resolve each element from exactly one place. Both halves fail silently if
//! they fail at all — a rule that stopped at the first absence still reports a
//! violation and still blocks the publish, and a required field satisfied from
//! the wrong home still passes — so each is asserted rather than inferred from
//! "the publish was blocked".

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;

use uuid::Uuid;

use super::{DescriptorSetComplete, V1_REQUIRED_DESCRIPTORS};
use crate::domain::instant::utc_ymd_hms;
use crate::domain::plan_rules::DESCRIPTOR_INCOMPLETE;
use crate::domain::plan_shape::{DescriptorSet, PlanShape};
use crate::domain::scope_key::PlanId;
use crate::domain::validation::{ValidationReport, ValidationRule};

fn plan_with(descriptors: Option<DescriptorSet>) -> PlanShape {
    let mut shape = PlanShape::new(
        PlanId::new(Uuid::from_u128(0x9_1a4)),
        3,
        utc_ymd_hms(2026, 8, 2, 12, 0, 0),
    );
    shape.descriptor_set = descriptors;
    shape
}

fn complete() -> DescriptorSet {
    DescriptorSet {
        invoice_line_template: Some("Subscription: {plan}".to_owned()),
        gl_code: Some("4000".to_owned()),
        itemization_rule: Some("per_plan".to_owned()),
        additional: BTreeMap::new(),
    }
}

fn run(rule: &DescriptorSetComplete, shape: &PlanShape) -> ValidationReport {
    let mut report = ValidationReport::default();
    rule.evaluate(shape, &mut report);
    report
}

/// Every violation this rule raises, as the fields it named.
fn missing_fields(report: &ValidationReport) -> Vec<String> {
    report
        .violations
        .iter()
        .map(|violation| {
            assert_eq!(violation.code, DESCRIPTOR_INCOMPLETE);
            violation
                .detail
                .split_whitespace()
                .next()
                .expect("the detail names the field first")
                .to_owned()
        })
        .collect()
}

#[test]
fn a_complete_set_is_silent() {
    let report = run(
        &DescriptorSetComplete::default(),
        &plan_with(Some(complete())),
    );

    assert!(report.violations.is_empty(), "got: {report:?}");
    assert!(report.is_publishable());
}

#[test]
fn each_missing_field_is_reported_by_its_own_name() {
    // One at a time, so a rule that reported a fixed string rather than the
    // field it found missing cannot pass.
    for (cleared, expected) in [
        (
            DescriptorSet {
                invoice_line_template: None,
                ..complete()
            },
            "invoiceLineTemplate",
        ),
        (
            DescriptorSet {
                gl_code: None,
                ..complete()
            },
            "glCode",
        ),
        (
            DescriptorSet {
                itemization_rule: None,
                ..complete()
            },
            "itemizationRule",
        ),
    ] {
        let report = run(&DescriptorSetComplete::default(), &plan_with(Some(cleared)));
        assert_eq!(missing_fields(&report), vec![expected.to_owned()]);
    }
}

/// The fail-closed contract's whole point, at rule scale: an author remediates
/// a plan in one pass, so a set missing three fields owes three lines.
#[test]
fn three_missing_fields_are_three_findings_not_the_first_of_them() {
    let report = run(
        &DescriptorSetComplete::default(),
        &plan_with(Some(DescriptorSet::default())),
    );

    assert_eq!(
        missing_fields(&report),
        vec!["invoiceLineTemplate", "glCode", "itemizationRule"],
        "every missing element, in the order D-48 v1 names them"
    );
}

#[test]
fn an_absent_set_reports_all_three_the_same_way_an_empty_one_does() {
    let absent = run(&DescriptorSetComplete::default(), &plan_with(None));
    let empty = run(
        &DescriptorSetComplete::default(),
        &plan_with(Some(DescriptorSet::default())),
    );

    // "You attached nothing" is not a shorter way of saying what is missing:
    // the author is owed the list either way, and a publish blocked with one
    // line would send them round the pipeline twice more.
    assert_eq!(missing_fields(&absent), missing_fields(&empty));
    assert_eq!(absent.violations.len(), 3);
}

/// A whitespace-only value is an absence wearing a value's shape. It matters
/// more here than for `planTier`: a blank `glCode` freezes into the snapshot and
/// an ERP posts against it.
#[test]
fn a_blank_value_is_not_a_declaration() {
    let report = run(
        &DescriptorSetComplete::default(),
        &plan_with(Some(DescriptorSet {
            gl_code: Some("   ".to_owned()),
            ..complete()
        })),
    );

    assert_eq!(missing_fields(&report), vec!["glCode".to_owned()]);
}

/// P5: a tenant requires a fourth descriptor by naming it in their
/// `pricing_policy_object` entry, and carries its value in `additional` — no
/// migration, no new column, no new wire code (D-152).
#[test]
fn a_configured_fourth_requirement_is_satisfied_from_the_extra_fields() {
    let rule = DescriptorSetComplete::extending_v1(["costCentre".to_owned()]);

    let unsatisfied = run(&rule, &plan_with(Some(complete())));
    assert_eq!(missing_fields(&unsatisfied), vec!["costCentre".to_owned()]);

    let satisfied = run(
        &rule,
        &plan_with(Some(DescriptorSet {
            additional: BTreeMap::from([("costCentre".to_owned(), "emea-ops".to_owned())]),
            ..complete()
        })),
    );
    assert!(satisfied.violations.is_empty(), "got: {satisfied:?}");
}

/// A field with a column of its own is read from that column and from nowhere
/// else, so one descriptor never has two homes: a `glCode` key in the extras
/// does not satisfy the `glCode` requirement.
#[test]
fn an_extra_field_may_not_stand_in_for_a_named_column() {
    let report = run(
        &DescriptorSetComplete::default(),
        &plan_with(Some(DescriptorSet {
            gl_code: None,
            additional: BTreeMap::from([("glCode".to_owned(), "4000".to_owned())]),
            ..complete()
        })),
    );

    assert_eq!(missing_fields(&report), vec!["glCode".to_owned()]);
}

/// A tenant entry that names one key twice is a typo, and reporting one absence
/// twice reads as two faults to whoever has to fix it.
#[test]
fn a_duplicated_requirement_is_one_requirement() {
    let rule = DescriptorSetComplete::extending_v1([
        "costCentre".to_owned(),
        "costCentre".to_owned(),
        "profitCentre".to_owned(),
    ]);

    assert_eq!(
        rule.required(),
        [
            "invoiceLineTemplate",
            "glCode",
            "itemizationRule",
            "costCentre",
            "profitCentre"
        ]
    );
    let report = run(&rule, &plan_with(Some(complete())));
    assert_eq!(
        missing_fields(&report),
        vec!["costCentre".to_owned(), "profitCentre".to_owned()],
        "first-seen order is kept, and the duplicate is not a second finding"
    );
}

/// D-152: the extension is **additive-only** over the pinned three. Naming a v1
/// field in a tenant entry can neither duplicate it nor move it out of its own
/// column, so no tenant configuration can satisfy `glCode` from the extras or
/// reorder what the report names first.
#[test]
fn naming_a_v1_field_in_the_extension_neither_duplicates_nor_moves_it() {
    let rule = DescriptorSetComplete::extending_v1(["glCode".to_owned(), "costCentre".to_owned()]);

    assert_eq!(
        rule.required(),
        [
            "invoiceLineTemplate",
            "glCode",
            "itemizationRule",
            "costCentre"
        ]
    );

    let report = run(
        &rule,
        &plan_with(Some(DescriptorSet {
            gl_code: None,
            additional: BTreeMap::from([
                ("glCode".to_owned(), "4000".to_owned()),
                ("costCentre".to_owned(), "emea-ops".to_owned()),
            ]),
            ..complete()
        })),
    );
    assert_eq!(
        missing_fields(&report),
        vec!["glCode".to_owned()],
        "the extras still do not stand in for a field with a column of its own"
    );
}

/// The v1 default is the documented contract, and **no tenant entry can require
/// less than it**: the only constructor adds. A per-tenant policy that could drop
/// `glCode` would publish past a pinned element of the contract Billing
/// countersigns.
#[test]
fn the_required_set_can_only_ever_grow_past_the_v1_contract() {
    assert_eq!(
        DescriptorSetComplete::default().required(),
        V1_REQUIRED_DESCRIPTORS
    );

    let no_extension = DescriptorSetComplete::extending_v1(Vec::new());
    assert_eq!(no_extension, DescriptorSetComplete::default());
    assert_eq!(
        run(&no_extension, &plan_with(None)).violations.len(),
        V1_REQUIRED_DESCRIPTORS.len(),
        "a tenant that configures nothing is still held to all three"
    );
}

/// `billingTiming` and `taxCategory` are the other two elements of D-48 v1 and
/// are **not** this rule's: the first is Slice 6's registered rule, the second is
/// each row's Slice-4 `tax_category_ref` (D-110). A second owner here would be
/// free to disagree with the first, invisibly.
#[test]
fn the_two_row_borne_elements_are_not_checked_here() {
    let required = DescriptorSetComplete::default();

    assert_eq!(required.required().len(), 3, "three of D-48 v1's five");
    assert!(
        !required
            .required()
            .iter()
            .any(|field| field == "billingTiming" || field == "taxCategory"),
        "neither row-borne element may be re-registered on the plan"
    );
    // And a complete descriptor set publishes: `taxCategory` is owned by
    // `domain::tax_display::TaxBasisComplete`, whose category arm blocks publish
    // when the effective category resolves to nothing (D-154). Re-registering the
    // element on the plan would be a second owner of the one rule.
    assert!(
        run(&required, &plan_with(Some(complete())))
            .violations
            .is_empty()
    );
}

/// **A column added to `DescriptorSet` is a failure here, not a silent absence.**
///
/// The roster pairs a wire name with the column it reads, and nothing in the type
/// system ties it to the struct — a fourth column would compile with no entry, and
/// then be required by no rule and read from nowhere. The destructure is what makes
/// that a red test: adding a field breaks this pattern before it breaks anything an
/// author would notice.
#[test]
fn every_v1_column_is_in_the_roster() {
    let super::DescriptorSet {
        invoice_line_template: _,
        gl_code: _,
        itemization_rule: _,
        additional: _,
    } = super::DescriptorSet::default();

    let set = super::DescriptorSet {
        invoice_line_template: Some("t".to_owned()),
        gl_code: Some("g".to_owned()),
        itemization_rule: Some("i".to_owned()),
        ..Default::default()
    };

    for name in super::V1_REQUIRED_DESCRIPTORS {
        assert!(
            super::declared(&set, name).is_some(),
            "{name} is required and must be read from its own column"
        );
    }
}
