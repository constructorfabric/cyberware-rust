//! Unit tests for the collection read's `$filter`/`$orderby`/`$select`
//! validation (ADR-0005/ADR-0004).

use time::macros::datetime;
use toolkit_odata::{ODataOrderBy, OrderKey, SortDir};

use super::*;
use crate::domain::error::DomainError;

fn parse(raw: &str) -> Result<ParsedFilter, DomainError> {
    let parsed = toolkit_odata::parse_filter_string(raw).expect("valid OData syntax");
    parse_filter(parsed.as_expr())
}

fn reason_of(err: &DomainError) -> &'static str {
    match err {
        DomainError::InvalidRequest { reason, .. } => reason,
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[test]
fn reference_eq_and_in_are_sql_clamps() {
    let f = parse("reference eq 'openai-key'").expect("valid");
    assert_eq!(f.reference_in, Some(vec!["openai-key".to_owned()]));
    assert!(f.type_uuid_in.is_none());

    let f = parse("reference in ('a', 'b', 'c')").expect("valid");
    assert_eq!(
        f.reference_in,
        Some(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()])
    );
}

#[test]
fn reference_rejects_other_operators() {
    let err = parse("startswith(reference, 'op')").expect_err("startswith must be rejected");
    assert_eq!(reason_of(&err), reasons::INVALID_FILTER);
}

#[test]
fn type_eq_parses_the_full_gts_id_into_a_uuid() {
    let gts_id = "gts.cf.core.credstore.credential.v1~cf.core.credstore.generic.v1~";
    let f = parse(&format!("type eq '{gts_id}'")).expect("valid");
    let expected = credstore_sdk::GtsId::try_new(gts_id)
        .expect("valid")
        .to_uuid();
    assert_eq!(f.type_uuid_in, Some(vec![expected]));
}

#[test]
fn type_rejects_an_unparseable_gts_id() {
    let err = parse("type eq 'not-a-gts-id'").expect_err("must reject");
    assert_eq!(reason_of(&err), reasons::INVALID_FILTER);
}

#[test]
fn duplicate_field_is_rejected() {
    let err =
        parse("reference eq 'a' and reference eq 'b'").expect_err("duplicate must be rejected");
    assert_eq!(reason_of(&err), reasons::INVALID_FILTER);
}

#[test]
fn or_and_not_are_rejected() {
    assert!(parse("reference eq 'a' or reference eq 'b'").is_err());
    assert!(parse("not (reference eq 'a')").is_err());
}

#[test]
fn sharing_and_fallback_parse_known_wire_values() {
    let f = parse("sharing eq 'shared'").expect("valid");
    assert_eq!(f.sharing_eq, Some(SharingMode::Shared));

    let f = parse("fallback eq 'none'").expect("valid");
    assert_eq!(f.fallback_eq, Some(Fallback::None));

    let err = parse("sharing eq 'bogus'").expect_err("must reject");
    assert_eq!(reason_of(&err), reasons::INVALID_FILTER);
}

#[test]
fn expires_at_supports_ordering_comparators() {
    let f = parse("expires_at gt 2030-01-01T00:00:00Z").expect("valid");
    let (op, at) = f.expires_at.expect("present");
    assert_eq!(op, toolkit_odata::filter::FilterOp::Gt);
    assert_eq!(at, datetime!(2030-01-01 0:00 UTC));
}

#[test]
fn combined_reference_and_sharing_filter_parses_both() {
    let f = parse("reference eq 'r' and sharing eq 'tenant'").expect("valid");
    assert_eq!(f.reference_in, Some(vec!["r".to_owned()]));
    assert_eq!(f.sharing_eq, Some(SharingMode::Tenant));
}

#[test]
fn matches_post_reduction_applies_sharing_fallback_expires_at() {
    let f = parse("sharing eq 'shared'").expect("valid");
    assert!(f.matches_post_reduction(SharingMode::Shared, None, None));
    assert!(!f.matches_post_reduction(SharingMode::Tenant, None, None));

    let f = parse("fallback eq 'none'").expect("valid");
    assert!(f.matches_post_reduction(SharingMode::Tenant, Some(Fallback::None), None));
    assert!(!f.matches_post_reduction(SharingMode::Tenant, Some(Fallback::Inherit), None));
    // No own row (fallback: None) never matches a fallback predicate.
    assert!(!f.matches_post_reduction(SharingMode::Tenant, None, None));

    let no_filter = ParsedFilter::default();
    assert!(no_filter.matches_post_reduction(SharingMode::Private, None, None));
}

#[test]
fn value_mode_selector_accepts_exactly_reference_or_type() {
    let f = parse("reference eq 'r'").expect("valid");
    assert!(f.require_value_mode_selector().is_ok());

    let f = parse("type eq 'gts.cf.core.credstore.credential.v1~cf.core.credstore.generic.v1~'")
        .expect("valid");
    assert!(f.require_value_mode_selector().is_ok());

    let f = parse("sharing eq 'shared'").expect("valid");
    let err = f.require_value_mode_selector().expect_err("must reject");
    assert_eq!(reason_of(&err), reasons::VALUE_MODE_SELECTOR);

    let neither = ParsedFilter::default();
    assert!(neither.require_value_mode_selector().is_err());
}

#[test]
fn select_allowlist_accepts_credential_fields_and_secret() {
    for field in [
        "reference",
        "type",
        "sharing",
        "status",
        "fallback",
        "expires_at",
        "inheritance",
        "version",
        "updated_at",
        "secret",
    ] {
        assert!(validate_select(&[field.to_owned()]).is_ok(), "{field}");
    }
    let err = validate_select(&["bogus".to_owned()]).expect_err("must reject");
    assert_eq!(reason_of(&err), reasons::INVALID_SELECT);
}

#[test]
fn is_value_mode_detects_secret_in_select() {
    assert!(is_value_mode(Some(&[
        "reference".to_owned(),
        "secret".to_owned()
    ])));
    assert!(!is_value_mode(Some(&["reference".to_owned()])));
    assert!(!is_value_mode(None));
}

#[test]
fn orderby_defaults_to_ascending_when_absent() {
    let dir = validate_metadata_orderby(&ODataOrderBy::empty()).expect("valid");
    assert_eq!(dir, ListDirection::Asc);
}

#[test]
fn orderby_accepts_reference_asc_and_desc() {
    let asc = ODataOrderBy(vec![OrderKey {
        field: "reference".to_owned(),
        dir: SortDir::Asc,
    }]);
    assert_eq!(
        validate_metadata_orderby(&asc).expect("valid"),
        ListDirection::Asc
    );

    let desc = ODataOrderBy(vec![OrderKey {
        field: "reference".to_owned(),
        dir: SortDir::Desc,
    }]);
    assert_eq!(
        validate_metadata_orderby(&desc).expect("valid"),
        ListDirection::Desc
    );
}

#[test]
fn orderby_rejects_any_other_field_or_multiple_keys() {
    let other = ODataOrderBy(vec![OrderKey {
        field: "updated_at".to_owned(),
        dir: SortDir::Asc,
    }]);
    let err = validate_metadata_orderby(&other).expect_err("must reject");
    assert_eq!(reason_of(&err), reasons::INVALID_ORDERBY_FIELD);

    let multiple = ODataOrderBy(vec![
        OrderKey {
            field: "reference".to_owned(),
            dir: SortDir::Asc,
        },
        OrderKey {
            field: "reference".to_owned(),
            dir: SortDir::Desc,
        },
    ]);
    assert!(validate_metadata_orderby(&multiple).is_err());
}
