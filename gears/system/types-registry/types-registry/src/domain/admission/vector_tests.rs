//! The comparison half of the revision vector, with no database in sight.

use super::*;

const BASE: &str = "acme.crm.base.type.v1~";
const DERIVED: &str = "acme.crm.base.type.v1~acme.crm.child.type.v1~";

fn dependency(gts_id: &str, resource_version: i64) -> VectorEntry {
    VectorEntry {
        gts_id: gts_id.to_owned(),
        role: VectorRole::Dependency,
        resource_version,
        resolution_fingerprint: None,
    }
}

fn dependent(gts_id: &str, resource_version: i64, fingerprint: u8) -> VectorEntry {
    VectorEntry {
        gts_id: gts_id.to_owned(),
        role: VectorRole::Dependent,
        resource_version,
        resolution_fingerprint: Some(vec![fingerprint]),
    }
}

fn vector(entries: Vec<VectorEntry>) -> RevisionVector {
    // The constructor sorts, so order in the fixtures below is free — and the invariant the private
    // field exists to protect is the constructor's job, not each test's.
    RevisionVector::new(vec![BASE.to_owned()], entries)
}

#[test]
fn two_identical_vectors_do_not_drift() {
    let recorded = vector(vec![dependency(BASE, 3), dependent(DERIVED, 7, 0xAA)]);
    assert_eq!(recorded.drift(&recorded.clone()), None);
}

#[test]
fn two_empty_vectors_do_not_drift() {
    assert_eq!(
        RevisionVector::default().drift(&RevisionVector::default()),
        None
    );
}

#[test]
fn a_dependency_whose_version_moved_is_reported_as_moved() {
    let recorded = vector(vec![dependency(BASE, 3)]);
    let fresh = vector(vec![dependency(BASE, 4)]);
    assert_eq!(
        recorded.drift(&fresh),
        Some(VectorDrift::Moved {
            gts_id: BASE.to_owned(),
            role: VectorRole::Dependency,
            recorded: 3,
            found: 4,
        })
    );
}

#[test]
fn a_dependent_whose_fingerprint_moved_under_a_still_version_is_reported_as_refreshed() {
    let recorded = vector(vec![dependent(DERIVED, 7, 0xAA)]);
    let fresh = vector(vec![dependent(DERIVED, 7, 0xBB)]);
    assert_eq!(
        recorded.drift(&fresh),
        Some(VectorDrift::Refreshed {
            gts_id: DERIVED.to_owned(),
        })
    );
}

#[test]
fn a_revision_is_reported_as_moved_not_refreshed() {
    let recorded = vector(vec![dependent(DERIVED, 7, 0xAA)]);
    let fresh = vector(vec![dependent(DERIVED, 8, 0xBB)]);
    assert!(matches!(
        recorded.drift(&fresh),
        Some(VectorDrift::Moved { .. })
    ));
}

#[test]
fn a_phantom_dependent_is_reported_as_appeared() {
    let recorded = vector(vec![dependency(BASE, 3)]);
    let fresh = vector(vec![dependency(BASE, 3), dependent(DERIVED, 1, 0xAA)]);
    assert_eq!(
        recorded.drift(&fresh),
        Some(VectorDrift::Appeared {
            gts_id: DERIVED.to_owned(),
            role: VectorRole::Dependent,
        })
    );
}

#[test]
fn a_dependency_that_is_gone_is_reported_as_vanished() {
    let recorded = vector(vec![dependency(BASE, 3), dependency(DERIVED, 1)]);
    let fresh = vector(vec![dependency(BASE, 3)]);
    assert_eq!(
        recorded.drift(&fresh),
        Some(VectorDrift::Vanished {
            gts_id: DERIVED.to_owned(),
            role: VectorRole::Dependency,
        })
    );
}

#[test]
fn an_entity_that_changed_role_reports_the_side_it_left() {
    let recorded = vector(vec![dependency(BASE, 3)]);
    let fresh = vector(vec![dependent(BASE, 3, 0xAA)]);
    assert_eq!(
        recorded.drift(&fresh),
        Some(VectorDrift::Vanished {
            gts_id: BASE.to_owned(),
            role: VectorRole::Dependency,
        })
    );
}

#[test]
fn the_first_difference_in_canonical_order_is_the_one_reported() {
    let recorded = vector(vec![dependency(BASE, 3), dependency(DERIVED, 1)]);
    let fresh = vector(vec![dependency(BASE, 9), dependency(DERIVED, 9)]);
    assert_eq!(
        recorded.drift(&fresh),
        Some(VectorDrift::Moved {
            gts_id: BASE.to_owned(),
            role: VectorRole::Dependency,
            recorded: 3,
            found: 9,
        })
    );
}

#[test]
fn every_drift_shape_says_what_happened() {
    assert_eq!(
        VectorDrift::Moved {
            gts_id: BASE.to_owned(),
            role: VectorRole::Dependency,
            recorded: 3,
            found: 4,
        }
        .to_string(),
        format!("dependency '{BASE}' moved from resource_version 3 to 4 after evaluation")
    );
    assert_eq!(
        VectorDrift::Appeared {
            gts_id: DERIVED.to_owned(),
            role: VectorRole::Dependent,
        }
        .to_string(),
        format!("dependent '{DERIVED}' appeared after evaluation")
    );
    assert_eq!(
        VectorDrift::Vanished {
            gts_id: BASE.to_owned(),
            role: VectorRole::Dependency,
        }
        .to_string(),
        format!("dependency '{BASE}' disappeared after evaluation")
    );
    assert_eq!(
        VectorDrift::Refreshed {
            gts_id: DERIVED.to_owned(),
        }
        .to_string(),
        format!("dependent '{DERIVED}' had its effective artifacts refreshed after evaluation")
    );
}
