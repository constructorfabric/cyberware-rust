//! Unit tests for the two hand-written `FilterField` impls.
//!
//! The rest of this module is derived, and the derive is covered by the
//! toolkit's own tests. These two are written by hand for one reason — the wire
//! field is `type`, a Rust keyword — and that reason is exactly what is asserted
//! here: the derive would have turned `r#type` into the name `r#type` and broken
//! `$filter=type eq '…'` on a list nobody would have thought to re-check.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use toolkit_odata::filter::{FieldKind, FilterField};

use super::{ExceptionFilterField, ExceptionOrderField};

// The keyword field, and the whole point of hand-writing these two impls. A
// derived name would carry the `r#` escape into the query string.
#[test]
fn the_keyword_field_is_spelled_type_on_the_wire() {
    assert_eq!(ExceptionFilterField::ExceptionType.name(), "type");
    assert_eq!(ExceptionOrderField::ExceptionType.name(), "type");
    for field in ExceptionFilterField::FIELDS {
        assert!(
            !field.name().contains('#'),
            "no wire name carries a raw-identifier escape: {}",
            field.name()
        );
    }
}

// Every field's name and kind, enumerated from `FIELDS` rather than listed here:
// a variant added to the enum and forgotten in `name()` or `kind()` is a compile
// error in those matches, but a variant left out of `FIELDS` is not, and `FIELDS`
// is what the query parser offers a caller.
#[test]
fn the_filter_roster_names_every_field_with_its_kind() {
    let roster: Vec<(&str, FieldKind)> = ExceptionFilterField::FIELDS
        .iter()
        .map(|f| (f.name(), f.kind()))
        .collect();
    assert_eq!(
        roster,
        vec![
            ("tenant_id", FieldKind::Uuid),
            ("exception_id", FieldKind::Uuid),
            ("type", FieldKind::String),
            ("status", FieldKind::String),
            ("business_ref", FieldKind::String),
            ("period_id", FieldKind::String),
        ]
    );
}

// The order roster is deliberately NARROWER than the filter one: `period_id` is
// filterable and not orderable, which the mapper refuses separately. Asserted as
// a set difference rather than as a second literal list, so the claim is the
// narrowing itself and not two lists that happen to differ.
#[test]
fn the_order_roster_is_the_filter_roster_without_period_id() {
    let filterable: Vec<&str> = ExceptionFilterField::FIELDS
        .iter()
        .map(FilterField::name)
        .collect();
    let orderable: Vec<&str> = ExceptionOrderField::FIELDS
        .iter()
        .map(FilterField::name)
        .collect();

    let missing: Vec<&str> = filterable
        .iter()
        .filter(|name| !orderable.contains(*name))
        .copied()
        .collect();
    assert_eq!(
        missing,
        vec!["period_id"],
        "exactly one field is filterable but not orderable"
    );
    assert_eq!(
        orderable,
        vec![
            "tenant_id",
            "exception_id",
            "type",
            "status",
            "business_ref"
        ]
    );
}

// The kinds of the order roster, for the filter roster's reason: a field typed
// `String` where the column is a uuid is a filter that silently matches nothing.
#[test]
fn the_order_roster_carries_the_same_kinds_as_its_filter_twin() {
    for field in ExceptionOrderField::FIELDS {
        let twin = ExceptionFilterField::FIELDS
            .iter()
            .find(|f| f.name() == field.name())
            .expect("every orderable field is filterable");
        assert_eq!(
            field.kind(),
            twin.kind(),
            "`{}` is typed differently on the two rosters",
            field.name()
        );
    }
}
