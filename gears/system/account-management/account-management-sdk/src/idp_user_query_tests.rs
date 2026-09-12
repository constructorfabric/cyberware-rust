//! Unit tests for [`ListUsersQuery::with_ids`], the bounded ID-set lookup.
//!
//! Its own file rather than a block in `idp_user_tests.rs`: the constructor is
//! this PR's addition, and keeping it apart leaves that file — and the
//! password-redaction tests it holds — untouched.

#![allow(clippy::expect_used, clippy::unwrap_used, reason = "test helpers")]

use super::*;

/// The set is a set: duplicates collapse, order is the sorted one, and each ID
/// reaches the filter as a typed `Uuid` rather than as its text — a string
/// there would compare against a `uuid` column and match nothing.
#[test]
fn user_id_set_lookup_deduplicates_and_preserves_a_typed_uuid_filter() {
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);
    let query = ListUsersQuery::with_ids([second, first, second]).expect("bounded ID set");
    assert_eq!(query.pagination.top(), 2);
    assert!(query.pagination.cursor().is_none());
    assert!(
        matches!(query.filter, Some(toolkit_odata::filter::FilterNode::InList {
        field: IdpUserFilterField::Id,
        values,
    }) if matches!(values.as_slice(), [toolkit_odata::filter::ODataValue::Uuid(a), toolkit_odata::filter::ODataValue::Uuid(b)] if *a == first && *b == second))
    );
}

/// Both bounds, because the lookup borrows the pagination's `top` for its size:
/// an empty set has no positive `top` to carry, and one past `MAX_TOP` must be
/// refused here rather than chunked silently by a provider.
#[test]
fn user_id_set_lookup_rejects_empty_and_oversized_batches() {
    assert!(matches!(
        ListUsersQuery::with_ids([]),
        Err(IdpUserPaginationError::TopMustBePositive)
    ));
    assert!(ListUsersQuery::with_ids((1..=200).map(Uuid::from_u128)).is_ok());
    assert!(matches!(
        ListUsersQuery::with_ids((1..=201).map(Uuid::from_u128)),
        Err(IdpUserPaginationError::TopExceedsMax {
            requested: 201,
            max: 200
        })
    ));
}
