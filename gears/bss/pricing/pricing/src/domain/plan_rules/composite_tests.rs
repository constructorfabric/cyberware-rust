//! One case per clause of the two composite rules that have an operand.
//!
//! The publication half of `inst-cm-constituents` has no case because it has no
//! rule: this gear holds no registry client, so there is nothing to assert. See
//! the module doc.

use serde_json::json;
use uuid::Uuid;

use super::{CompositeArity, CompositeSelfReference};
use crate::domain::plan_shape::{CompositeMeter, PlanShape};
use crate::domain::rules::{COMPOSITE_SELF_REFERENCE, COMPOSITE_TOO_FEW_CONSTITUENTS};
use crate::domain::scope_key::PlanId;
use crate::domain::validation::{ValidationReport, ValidationRule};
use time::OffsetDateTime;

fn composite(output: &str, units: &[&str]) -> CompositeMeter {
    CompositeMeter {
        composite_id: Uuid::from_u128(u128::from(output.len() as u64) + 0xc0_00),
        output_unit: output.to_owned(),
        constituent_units: units.iter().map(|u| (*u).to_owned()).collect(),
        formula: json!({ "op": "weighted_sum" }),
    }
}

fn shape(composites: Vec<CompositeMeter>) -> PlanShape {
    let mut shape = PlanShape::new(
        PlanId::new(Uuid::from_u128(1)),
        0,
        OffsetDateTime::now_utc(),
    );
    shape.composites = composites;
    shape
}

fn report_of(rule: &dyn ValidationRule<PlanShape>, subject: &PlanShape) -> ValidationReport {
    let mut report = ValidationReport::default();
    rule.evaluate(subject, &mut report);
    report
}

#[test]
fn two_constituents_is_the_floor_and_one_is_not_a_composite() {
    let report = report_of(&CompositeArity, &shape(vec![composite("vm", &["vcpu"])]));
    assert_eq!(report.violations.len(), 1);
    assert_eq!(report.violations[0].code, COMPOSITE_TOO_FEW_CONSTITUENTS);

    let ok = report_of(
        &CompositeArity,
        &shape(vec![composite("vm", &["vcpu", "ram"])]),
    );
    assert!(ok.violations.is_empty());
}

/// **A constituent named twice is one constituent, and the length test could not
/// see it.**
///
/// `["vcpu", "vcpu"]` has length two and names *one* meter, so
/// `constituent_units.len() < 2` passed it. What it admits is a composite that
/// derives `vm` from `vcpu` alone — the one-level-of-indirection-that-changes-no-
/// charge this rule was written to refuse — wearing a duplicate as a disguise.
/// Counting entries answers "how many did the author type";
/// `inst-cm-constituents` asks how many meters are priced **together**, and that
/// is the distinct set.
///
/// No column can catch it either: the unique index on this table is
/// `(tenant, plan, revision, output_unit)`, and `constituent_units` is an opaque
/// `jsonb` array with no `CHECK` over its contents (`pricing_composite_meter`).
#[test]
fn a_constituent_named_twice_is_still_one_constituent() {
    let report = report_of(
        &CompositeArity,
        &shape(vec![composite("vm", &["vcpu", "vcpu"])]),
    );
    assert_eq!(
        report.violations.len(),
        1,
        "two entries naming one unit is a one-constituent composite"
    );
    assert_eq!(report.violations[0].code, COMPOSITE_TOO_FEW_CONSTITUENTS);

    // And the distinct count is what the operator is told, so the message names
    // the number that has to change rather than the number they typed.
    assert!(
        report.violations[0].detail.contains("1 distinct"),
        "the refusal must name the distinct count: {}",
        report.violations[0].detail
    );
}

#[test]
fn a_direct_self_reference_is_refused() {
    let report = report_of(
        &CompositeSelfReference,
        &shape(vec![composite("vm", &["vcpu", "vm"])]),
    );
    assert_eq!(report.violations.len(), 1);
    assert_eq!(report.violations[0].code, COMPOSITE_SELF_REFERENCE);
}

/// **The transitive case, which is the one a row-local rule could never see.**
///
/// `vm` is built from `pod`, and `pod` from `vm`. Neither definition is
/// self-referential on its own; the cycle exists only across the pair, and §9
/// asks for it by name.
#[test]
fn a_transitive_self_reference_is_refused() {
    let report = report_of(
        &CompositeSelfReference,
        &shape(vec![
            composite("vm", &["vcpu", "pod"]),
            composite("pod", &["ram", "vm"]),
        ]),
    );
    assert_eq!(
        report.violations.len(),
        2,
        "both definitions are in the cycle, and an operator fixing either breaks it"
    );
    assert!(
        report
            .violations
            .iter()
            .all(|v| v.code == COMPOSITE_SELF_REFERENCE)
    );
}

/// A shared constituent is not a cycle — the walk must terminate and stay quiet.
#[test]
fn two_composites_sharing_a_constituent_are_fine() {
    let report = report_of(
        &CompositeSelfReference,
        &shape(vec![
            composite("vm", &["vcpu", "ram"]),
            composite("pod", &["vcpu", "disk"]),
        ]),
    );
    assert!(report.violations.is_empty());
}
