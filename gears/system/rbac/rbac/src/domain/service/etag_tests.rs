//! Unit tests for [`super::Etag`] / [`super::etag_for`].

use std::str::FromStr;

use chrono::{DateTime, SubsecRound, Utc};
use uuid::Uuid;

use super::{Etag, EtagParseError, etag_for};

fn sample_id() -> Uuid {
    Uuid::parse_str("01963b0e-0001-7000-8000-000000000001").expect("valid v7 uuid literal")
}

#[test]
fn etag_round_trips_through_string_form() {
    let ts: DateTime<Utc> = "2026-05-11T13:24:55.123456Z"
        .parse::<DateTime<Utc>>()
        .expect("rfc3339 with micros");
    let etag = etag_for(ts, sample_id());

    assert_eq!(
        etag.as_str(),
        "2026-05-11T13:24:55.123456Z:01963b0e-0001-7000-8000-000000000001"
    );

    let parsed = Etag::from_str(etag.as_str()).expect("round-trip parses");
    assert_eq!(parsed, etag, "round-trip via FromStr must be byte-stable");
}

#[test]
fn etag_truncates_nanosecond_residue_to_microseconds() {
    // Two values that differ only in their sub-microsecond tail must
    // collapse to the same ETag.
    let with_nanos: DateTime<Utc> = "2026-05-11T13:24:55.999999999Z"
        .parse::<DateTime<Utc>>()
        .expect("rfc3339 with nanos");
    let without_nanos = with_nanos.trunc_subsecs(6);
    assert_ne!(with_nanos, without_nanos, "preconditions: nanos differ");

    let id = sample_id();
    assert_eq!(
        etag_for(with_nanos, id),
        etag_for(without_nanos, id),
        "nano residue must not affect the etag \u{2014} PostgreSQL stores micros only"
    );
}

/// `Etag` equality is byte-exact over the whole validator string.
///
/// The uppercased form still PARSES — `uuid` accepts either case and the
/// timestamp's `T`/`Z` are already upper — so `FromStr` is not the guard here.
/// Equality is, and it has to be compared between two `Etag` values: asserting
/// that a string differs from its own `to_uppercase()` is a language tautology
/// that could not fail even if `Etag`'s `PartialEq` were case-insensitive.
#[test]
fn etag_comparison_is_case_sensitive() {
    let ts: DateTime<Utc> = "2026-05-11T13:24:55.123456Z"
        .parse::<DateTime<Utc>>()
        .expect("rfc3339 with micros");
    let etag = etag_for(ts, sample_id());

    let upper = Etag::from_str(&etag.as_str().to_uppercase())
        .expect("an uppercased validator is still well-formed");
    assert_ne!(
        upper, etag,
        "two Etag values differing only in case MUST NOT compare equal - an \
         If-Match is a byte-exact precondition"
    );
    // And the round-trip of the canonical form still matches itself, so the
    // assertion above is about case and not about parsing losing information.
    assert_eq!(
        Etag::from_str(etag.as_str()).expect("round-trip parses"),
        etag
    );
}

#[test]
fn from_str_rejects_missing_separator() {
    let err = Etag::from_str("not-an-etag").expect_err("MUST reject");
    assert_eq!(err, EtagParseError::MissingSeparator);
}

#[test]
fn from_str_rejects_invalid_timestamp() {
    let id = sample_id();
    let s = format!("not-a-timestamp:{id}");
    let err = Etag::from_str(&s).expect_err("MUST reject");
    assert_eq!(err, EtagParseError::InvalidTimestamp);
}

#[test]
fn from_str_rejects_invalid_uuid() {
    let s = "2026-05-11T13:24:55.123456Z:not-a-uuid";
    let err = Etag::from_str(s).expect_err("MUST reject");
    assert_eq!(err, EtagParseError::InvalidUuid);
}

#[test]
fn from_str_rejects_non_microsecond_precision_timestamp() {
    // The canonical form is micros-only — sub-microsecond precision must
    // be rejected. Hand-crafted because `etag_for` always truncates.
    let s = format!("2026-05-11T13:24:55.1234567Z:{}", sample_id());
    let err = Etag::from_str(&s).expect_err("MUST reject sub-micro precision");
    assert!(matches!(err, EtagParseError::InvalidTimestamp));
}

#[test]
fn etag_advances_when_updated_at_advances() {
    let id = sample_id();
    let a: DateTime<Utc> = "2026-05-11T13:24:55.123456Z".parse().expect("rfc3339");
    let b: DateTime<Utc> = "2026-05-11T13:24:55.123457Z".parse().expect("rfc3339");
    assert_ne!(
        etag_for(a, id),
        etag_for(b, id),
        "one-microsecond advance MUST advance the etag"
    );
}

#[test]
fn etag_differs_per_id_even_at_identical_timestamp() {
    let ts: DateTime<Utc> = "2026-05-11T13:24:55.123456Z".parse().expect("rfc3339");
    let a = Uuid::parse_str("01963b0e-0001-7000-8000-000000000001").expect("v7 uuid");
    let b = Uuid::parse_str("01963b0e-0002-7000-8000-000000000002").expect("v7 uuid");
    assert_ne!(etag_for(ts, a), etag_for(ts, b));
}
