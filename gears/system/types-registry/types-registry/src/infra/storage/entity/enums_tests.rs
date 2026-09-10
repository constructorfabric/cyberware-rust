//! Enum ↔ smallint round-trip, asserting the exact integers `database.sql`
//! writes beside each column.
//!
//! These numbers are a storage contract: `database.sql` makes the numbering
//! append-only after the first release, because renumbering is a data migration. A
//! variant reordered or inserted in the middle would silently reinterpret every
//! existing row, and nothing else in the build would notice — `DeriveActiveEnum`
//! compiles whatever `num_value` it is given.
//!
//! Every case is written out literally rather than derived from the variant order,
//! which would restate the bug it is meant to catch.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use sea_orm::ActiveEnum;

use super::{
    DependencyKind, EntityKind, LifecycleStatus, OperationItemStatus, OperationKind,
    OperationStatus, OwnershipScope, Plane,
};

/// Assert the stored integer of every variant, and that the value parses back to
/// the same variant. The list is exhaustive by construction: the caller passes
/// every variant, and a separate `EnumIter`-based count test below catches a
/// variant added without a case here.
fn assert_round_trip<T>(cases: &[(T, i16)])
where
    T: ActiveEnum<Value = i16> + PartialEq + std::fmt::Debug + Clone,
{
    for (variant, expected) in cases {
        assert_eq!(
            variant.clone().into_value(),
            *expected,
            "{variant:?} must store as {expected}"
        );
        let parsed = T::try_from_value(expected)
            .unwrap_or_else(|e| panic!("{expected} must parse back to {variant:?}: {e}"));
        assert_eq!(&parsed, variant, "{expected} parsed to the wrong variant");
    }
}

#[test]
fn ownership_scope_numbering() {
    assert_round_trip(&[(OwnershipScope::Global, 1), (OwnershipScope::Tenant, 2)]);
}

#[test]
fn entity_kind_numbering() {
    assert_round_trip(&[(EntityKind::TypeSchema, 1), (EntityKind::Instance, 2)]);
}

#[test]
fn lifecycle_status_numbering() {
    assert_round_trip(&[(LifecycleStatus::Active, 1), (LifecycleStatus::Deleted, 2)]);
}

#[test]
fn operation_kind_numbering() {
    assert_round_trip(&[
        (OperationKind::Registration, 1),
        (OperationKind::Deletion, 2),
    ]);
}

#[test]
fn dependency_kind_numbering() {
    assert_round_trip(&[
        (DependencyKind::SchemaRef, 1),
        (DependencyKind::Derivation, 2),
        (DependencyKind::InstanceOf, 3),
    ]);
    assert!(
        DependencyKind::try_from_value(&4).is_err(),
        "the vocabulary has three values",
    );
}

#[test]
fn plane_numbering() {
    assert_round_trip(&[(Plane::Platform, 1), (Plane::Tenant, 2)]);
}

#[test]
fn operation_status_numbering() {
    assert_round_trip(&[
        (OperationStatus::Pending, 1),
        (OperationStatus::Running, 2),
        (OperationStatus::Completed, 3),
    ]);
}

#[test]
fn operation_item_status_numbering() {
    assert_round_trip(&[
        (OperationItemStatus::Pending, 1),
        (OperationItemStatus::Running, 2),
        (OperationItemStatus::Succeeded, 3),
        (OperationItemStatus::Unchanged, 4),
        (OperationItemStatus::Failed, 5),
    ]);
}

/// A variant added without a case in the tests above would otherwise go
/// unnoticed. `EnumIter` gives the real count; the literals give the expected
/// one.
#[test]
fn no_vocabulary_has_grown_without_its_numbering_being_pinned() {
    use sea_orm::strum::IntoEnumIterator;
    assert_eq!(OwnershipScope::iter().count(), 2);
    assert_eq!(EntityKind::iter().count(), 2);
    assert_eq!(LifecycleStatus::iter().count(), 2);
    assert_eq!(OperationKind::iter().count(), 2);
    assert_eq!(Plane::iter().count(), 2);
    assert_eq!(OperationStatus::iter().count(), 3);
    assert_eq!(OperationItemStatus::iter().count(), 5);
    assert_eq!(DependencyKind::iter().count(), 3);
}

/// `database.sql`: *"Numbering is per column and deliberately NOT aligned
/// between columns. `3` is `completed` in operation.status and `succeeded` in
/// `operation_item.status`. Where two columns agree that is coincidence rather
/// than a contract and MUST NOT be turned into one."*
///
/// This test exists to make a future reader who notices the overlap stop and
/// read the comment instead of "unifying" the two enums.
#[test]
fn the_two_status_vocabularies_are_separate_types_that_happen_to_share_three() {
    assert_eq!(OperationStatus::Completed.into_value(), 3);
    assert_eq!(OperationItemStatus::Succeeded.into_value(), 3);
    // The vocabularies diverge immediately after: the operation has no fourth
    // value, the item has two.
    assert!(OperationStatus::try_from_value(&4).is_err());
    assert_eq!(
        OperationItemStatus::try_from_value(&4),
        Ok(OperationItemStatus::Unchanged)
    );
}

/// Every value outside its column's CHECK list must fail to parse, so a row
/// written by a future version with a higher number is a clean read error rather
/// than a silent misinterpretation.
#[test]
fn values_outside_each_vocabulary_are_rejected() {
    assert!(OwnershipScope::try_from_value(&0).is_err());
    assert!(OwnershipScope::try_from_value(&3).is_err());
    assert!(EntityKind::try_from_value(&3).is_err());
    assert!(LifecycleStatus::try_from_value(&3).is_err());
    assert!(OperationKind::try_from_value(&3).is_err());
    assert!(Plane::try_from_value(&3).is_err());
    assert!(OperationStatus::try_from_value(&0).is_err());
    assert!(OperationItemStatus::try_from_value(&6).is_err());
    assert!(DependencyKind::try_from_value(&5).is_err());
}
