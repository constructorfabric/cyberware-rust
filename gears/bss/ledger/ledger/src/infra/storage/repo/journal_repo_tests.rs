//! Unit tests for the pure `$orderby` helpers in `journal_repo.rs`.
//!
//! Both are pure rewrites of an [`ODataQuery`]'s order, so the whole contract
//! is testable without a database. The keyset walks they feed are exercised
//! end-to-end by the Postgres tier; what is checked here is the rewrite
//! itself, including the idempotence the cursor path depends on.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir};

use super::{query_with_default_order, query_with_unique_order};
use crate::odata::BalanceFilterField;

/// A caller `$orderby`, as the extractor would have parsed it.
fn ordered_by(field: &str) -> ODataQuery {
    ODataQuery::new().with_order(ODataOrderBy(vec![OrderKey {
        field: field.to_owned(),
        dir: SortDir::Asc,
    }]))
}

// ---------------------------------------------------------------------------
// query_with_default_order
// ---------------------------------------------------------------------------

#[test]
fn a_bare_list_gets_the_default_keyset_order() {
    let out = query_with_default_order(&ODataQuery::new(), "account_id");
    assert!(out.order.equals_signed_tokens("+account_id"));
}

#[test]
fn a_callers_orderby_is_left_alone() {
    let out = query_with_default_order(&ordered_by("balance_minor"), "account_id");
    assert!(out.order.equals_signed_tokens("+balance_minor"));
}

// ---------------------------------------------------------------------------
// query_with_unique_order
// ---------------------------------------------------------------------------

// The defect this exists for: `paginate_odata` appends exactly one tiebreaker,
// so a caller's `$orderby` would leave the balances walk ordered by
// `[balance_minor, account_id]` — and an account held in two currencies has two
// rows sharing that pair, one of which falls out at a page boundary. The
// `currency` suffix completes the `(tenant_id, account_id, currency)` key.
#[test]
fn the_suffix_completes_a_composite_key_behind_a_callers_orderby() {
    let out = query_with_unique_order(
        &ordered_by("balance_minor"),
        &[BalanceFilterField::Currency],
    );
    assert!(out.order.equals_signed_tokens("+balance_minor,+currency"));
}

#[test]
fn the_suffix_lands_behind_the_default_order_too() {
    let seeded = query_with_default_order(&ODataQuery::new(), "account_id");
    let out = query_with_unique_order(&seeded, &[BalanceFilterField::Currency]);
    assert!(out.order.equals_signed_tokens("+account_id,+currency"));
}

/// Page 2 rebuilds its order from `cursor.s`, which already carries the suffix,
/// and the helper runs again on that rebuilt query. A second application must
/// therefore be a no-op or every page would grow another `currency` key.
#[test]
fn applying_the_suffix_twice_changes_nothing() {
    let once = query_with_unique_order(
        &ordered_by("balance_minor"),
        &[BalanceFilterField::Currency],
    );
    let twice = query_with_unique_order(&once, &[BalanceFilterField::Currency]);
    // `ODataOrderBy` is not `PartialEq`; its `Display` is the readable form.
    assert_eq!(twice.order.to_string(), once.order.to_string());
    assert!(twice.order.equals_signed_tokens("+balance_minor,+currency"));
}

/// Same skip, reached the other way: a caller who already ordered by the
/// suffix field keeps their own direction rather than gaining a duplicate
/// ascending key behind it.
#[test]
fn a_suffix_field_the_caller_already_ordered_by_is_not_duplicated() {
    let descending = ODataQuery::new().with_order(ODataOrderBy(vec![OrderKey {
        field: "currency".to_owned(),
        dir: SortDir::Desc,
    }]));
    let out = query_with_unique_order(&descending, &[BalanceFilterField::Currency]);
    assert!(out.order.equals_signed_tokens("-currency"));
}

#[test]
fn an_empty_suffix_leaves_the_order_untouched() {
    let empty: [BalanceFilterField; 0] = [];
    let out = query_with_unique_order(&ordered_by("balance_minor"), &empty);
    assert!(out.order.equals_signed_tokens("+balance_minor"));
}

/// A multi-field suffix appends in the order given — the key halves of a
/// three-column primary key have to line up with the index, not arrive sorted.
#[test]
fn a_multi_field_suffix_keeps_the_order_it_was_given() {
    let out = query_with_unique_order(
        &ordered_by("balance_minor"),
        &[BalanceFilterField::AccountId, BalanceFilterField::Currency],
    );
    assert!(
        out.order
            .equals_signed_tokens("+balance_minor,+account_id,+currency")
    );
}

/// The suffix is spelled from the `FilterField` roster rather than a string
/// literal, so a renamed variant cannot silently order by a column the mapper
/// no longer recognises.
#[test]
fn the_suffix_field_names_come_from_the_filter_roster() {
    use toolkit_odata::filter::FilterField;
    assert_eq!(BalanceFilterField::Currency.name(), "currency");
    assert_eq!(BalanceFilterField::AccountId.name(), "account_id");
}

/// `$filter`, `limit` and the rest of the query ride through untouched — the
/// helper rewrites the order and nothing else.
#[test]
fn the_rest_of_the_query_rides_through() {
    let query = ordered_by("balance_minor").with_limit(7);
    let out = query_with_unique_order(&query, &[BalanceFilterField::Currency]);
    assert_eq!(out.limit, Some(7));
    assert!(out.cursor.is_none());
    assert!(out.filter.is_none());
}
