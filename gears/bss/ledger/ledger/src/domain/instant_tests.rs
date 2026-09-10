//! Unit tests for the `chrono` → `time` instant helpers.
//!
//! The pricing twin has these; this module shipped without them, so the doc
//! claims here — the exact chrono bytes, the microsecond preimage integer, the
//! `YYYYMM` and civil-date derivations — were asserted nowhere.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use chrono::NaiveDate;
use time::OffsetDateTime;

use super::{
    format_rfc3339, from_unix, from_unix_millis, parse_rfc3339, rfc3339, timestamp_micros,
    timestamp_millis, to_naive_date, try_days, try_utc_ymd_hms, utc_ymd_hms, yyyymm,
};

fn at(secs: i64, nanos: u32) -> OffsetDateTime {
    from_unix(secs, nanos).expect("a representable instant")
}

// The one rendering: a `Z` designator and a fraction of exactly six digits, on
// every arm. Each case is a width a variable-fraction renderer would print
// differently — `time`'s `Rfc3339` prints `.1` for the tenth and drops the
// fraction on the whole second, and chrono's `AutoSi` printed nine digits for
// the last one. The final arm is the truncation the microsecond form commits to:
// sub-microsecond precision has no representation downstream.
#[test]
fn the_rendering_is_six_digit_microseconds_with_a_z() {
    assert_eq!(
        format_rfc3339(at(1_757_000_000, 0)),
        "2025-09-04T15:33:20.000000Z",
        "a whole second still carries its six digits"
    );
    assert_eq!(
        format_rfc3339(at(1_757_000_000, 100_000_000)),
        "2025-09-04T15:33:20.100000Z",
        "a tenth of a second is `.100000`, not `.1`"
    );
    assert_eq!(
        format_rfc3339(at(1_757_000_000, 123_456_000)),
        "2025-09-04T15:33:20.123456Z"
    );
    assert_eq!(
        format_rfc3339(at(1_757_000_000, 123_456_789)),
        "2025-09-04T15:33:20.123456Z",
        "nanoseconds below the microsecond are truncated, not rounded"
    );
}

// A non-UTC input renders in UTC: the offset is folded in, not printed.
#[test]
fn the_rendering_normalises_to_utc_first() {
    let plus_two = at(1_757_000_000, 0).to_offset(time::UtcOffset::from_hms(2, 0, 0).unwrap());
    assert_eq!(format_rfc3339(plus_two), "2025-09-04T15:33:20.000000Z");
}

// The audit-chain preimage integer. The shipped vector uses whole seconds, so
// the sub-second arm — where a truncation or a rounding would silently change
// the hash — is pinned here.
#[test]
fn timestamp_micros_keeps_the_sub_second_digits() {
    assert_eq!(
        timestamp_micros(at(1_757_000_000, 0)),
        1_757_000_000_000_000
    );
    assert_eq!(
        timestamp_micros(at(1_757_000_000, 123_456_789)),
        1_757_000_000_123_456
    );
    assert_eq!(
        timestamp_millis(at(1_757_000_000, 123_456_789)),
        1_757_000_000_123
    );
}

// `yyyymm` is `u8::from(month)`, never the month's name: the defect this branch
// carried was `format!("{:02}", now.month())` printing `September`.
#[test]
fn yyyymm_is_six_digits_across_the_year_boundary() {
    assert_eq!(yyyymm(utc_ymd_hms(2026, 9, 8, 10, 0, 0)), "202609");
    assert_eq!(yyyymm(utc_ymd_hms(2026, 12, 31, 23, 59, 59)), "202612");
    assert_eq!(yyyymm(utc_ymd_hms(2027, 1, 1, 0, 0, 0)), "202701");
    let late_evening_west =
        utc_ymd_hms(2026, 1, 1, 1, 0, 0).to_offset(time::UtcOffset::from_hms(-3, 0, 0).unwrap());
    assert_eq!(
        yyyymm(late_evening_west),
        "202601",
        "the period is taken in UTC, not in the caller's offset"
    );
}

#[test]
fn to_naive_date_takes_the_utc_civil_date() {
    assert_eq!(
        to_naive_date(utc_ymd_hms(2026, 2, 28, 23, 59, 59)),
        NaiveDate::from_ymd_opt(2026, 2, 28).unwrap()
    );
    let just_past_midnight_east =
        utc_ymd_hms(2026, 3, 1, 0, 30, 0).to_offset(time::UtcOffset::from_hms(5, 0, 0).unwrap());
    assert_eq!(
        to_naive_date(just_past_midnight_east),
        NaiveDate::from_ymd_opt(2026, 3, 1).unwrap(),
        "the civil date is UTC's, not the offset's"
    );
}

// An invalid calendar value falls back to the epoch rather than panicking —
// the documented contract of `utc_ymd_hms`.
#[test]
fn an_impossible_civil_date_is_none_not_the_epoch() {
    assert_eq!(try_utc_ymd_hms(2026, 2, 30, 0, 0, 0), None);
    assert_eq!(try_utc_ymd_hms(2026, 13, 1, 0, 0, 0), None);
    assert_eq!(try_utc_ymd_hms(2026, 1, 1, 24, 0, 0), None);
    assert_eq!(
        try_utc_ymd_hms(2026, 2, 28, 23, 59, 59).map(timestamp_millis),
        Some(1_772_323_199_000)
    );
}

/// A fixture that names a date the calendar does not have fails where it is
/// written, not later as an epoch instant that satisfies every `<` bound.
#[test]
#[should_panic(expected = "is not a valid civil date-time")]
fn a_fixture_with_an_impossible_date_fails_where_it_is_written() {
    let _unreached = utc_ymd_hms(2026, 2, 30, 0, 0, 0);
}

// ---------------------------------------------------------------------------
// The serde boundary: every instant that enters is a UTC one.
// ---------------------------------------------------------------------------

/// A request shape with both field forms the DTOs use.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct Carrier {
    #[serde(with = "rfc3339")]
    at: OffsetDateTime,
    #[serde(default, with = "rfc3339::option")]
    maybe: Option<OffsetDateTime>,
}

/// The premise, measured on `time` itself: its own `rfc3339` keeps the written
/// offset, and the UTC form of this instant is year 10000, which `time` cannot
/// hold — so `to_offset(UTC)` on it (`to_naive_date`'s first line) panics.
#[test]
fn time_alone_admits_an_instant_that_has_no_utc_form() {
    let parsed = OffsetDateTime::parse(
        "9999-12-31T23:00:00-05:00",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("time's rfc3339 parses the local form");
    assert!(parsed.checked_to_offset(time::UtcOffset::UTC).is_none());
}

#[test]
fn a_request_instant_arrives_in_utc_whatever_offset_it_was_written_in() {
    let carrier: Carrier = serde_json::from_str(
        r#"{"at":"2026-03-01T05:00:00-05:00","maybe":"2026-03-01T05:00:00.25+02:00"}"#,
    )
    .expect("both instants are representable");
    assert_eq!(carrier.at.offset(), time::UtcOffset::UTC);
    assert_eq!(format_rfc3339(carrier.at), "2026-03-01T10:00:00.000000Z");
    assert_eq!(
        carrier.maybe.map(format_rfc3339),
        Some("2026-03-01T03:00:00.250000Z".to_owned())
    );
    assert_eq!(yyyymm(carrier.at), "202603");
}

#[test]
fn an_instant_with_no_utc_form_is_refused_at_the_boundary_not_at_the_first_render() {
    let err = serde_json::from_str::<Carrier>(r#"{"at":"9999-12-31T23:00:00-05:00"}"#)
        .expect_err("no UTC form");
    assert!(err.to_string().contains("no UTC representation"), "{err}");

    let err = serde_json::from_str::<Carrier>(
        r#"{"at":"2026-01-01T00:00:00Z","maybe":"9999-12-31T23:00:00-05:00"}"#,
    )
    .expect_err("the option form refuses it too");
    assert!(err.to_string().contains("no UTC representation"), "{err}");
}

#[test]
fn the_wire_form_is_the_gears_own_rendering_and_an_absent_option_reads_as_none() {
    let carrier: Carrier =
        serde_json::from_str(r#"{"at":"2026-01-01T00:00:00Z"}"#).expect("a bare instant");
    assert_eq!(carrier.maybe, None);
    // The serialized bytes are `format_rfc3339`'s, not `time::serde::rfc3339`'s:
    // a field written here and the same instant rendered into a key or a digest
    // elsewhere have to spell alike, which is what delegating to `time` gave up.
    assert_eq!(
        serde_json::to_string(&carrier).expect("serializes"),
        r#"{"at":"2026-01-01T00:00:00.000000Z","maybe":null}"#
    );
}

// ---------------------------------------------------------------------------
// The fallible constructors: every one answers `None` rather than panicking.
// ---------------------------------------------------------------------------

// `from_unix_millis` is the millisecond preimage of `timestamp_millis`, and the
// round trip is the claim: a value that survives one direction has to survive
// the other, or a persisted millisecond field reads back as a different instant.
#[test]
fn from_unix_millis_round_trips_timestamp_millis() {
    let millis = 1_757_000_000_123_i64;
    let at = from_unix_millis(millis).expect("a representable instant");
    assert_eq!(timestamp_millis(at), millis);
    assert_eq!(format_rfc3339(at), "2025-09-04T15:33:20.123000Z");

    // The epoch, because a sign error is invisible at any positive value.
    let epoch = from_unix_millis(0).expect("the epoch is an instant");
    assert_eq!(format_rfc3339(epoch), "1970-01-01T00:00:00.000000Z");
    assert_eq!(
        format_rfc3339(from_unix_millis(-1_000).expect("before the epoch")),
        "1969-12-31T23:59:59.000000Z",
        "a negative millisecond is an instant, not an error"
    );
}

// The out-of-range arm. `i64::MAX` milliseconds is some 292 million years past
// the epoch, which `time` cannot hold - so this is the case that distinguishes
// "answers None" from "panics", and the function exists to be the former.
#[test]
fn from_unix_millis_refuses_a_value_that_is_not_an_instant() {
    assert!(from_unix_millis(i64::MAX).is_none());
    assert!(from_unix_millis(i64::MIN).is_none());
}

// `try_days` replaced `chrono::Duration::try_days`, and its whole job is the
// overflow arm: the multiplication to seconds is what can wrap, and a wrapped
// duration would move a deadline rather than refuse to compute one.
#[test]
fn try_days_converts_and_refuses_the_overflow_it_exists_for() {
    assert_eq!(try_days(0), Some(time::Duration::seconds(0)));
    assert_eq!(try_days(1), Some(time::Duration::seconds(86_400)));
    assert_eq!(try_days(-30), Some(time::Duration::seconds(-2_592_000)));

    // Each of the three multiplications can overflow; these pick the value that
    // survives the earlier ones and fails the later, so no arm is unreachable.
    assert_eq!(try_days(i64::MAX), None, "the hours multiplication wraps");
    assert_eq!(try_days(i64::MIN), None);
    assert_eq!(
        try_days(i64::MAX / 3_600),
        None,
        "past the hours step, the minutes step is what refuses"
    );
}

// `parse_rfc3339` is the read side of `format_rfc3339`, so the round trip is the
// property. The refusal matters as much: a malformed stored instant must be an
// error the caller can class, never a silent default.
#[test]
fn parse_rfc3339_round_trips_the_rendering_and_refuses_what_is_not_one() {
    let rendered = "2025-09-04T15:33:20.123456Z";
    let parsed = parse_rfc3339(rendered).expect("its own rendering parses");
    assert_eq!(format_rfc3339(parsed), rendered);
    assert_eq!(parsed.offset(), time::UtcOffset::UTC);

    // An offset form parses and keeps its offset - normalising to UTC is the
    // serde boundary's job, not this function's, and conflating the two is how
    // an instant would arrive already folded where the caller wanted the local
    // form.
    let offset = parse_rfc3339("2026-03-01T05:00:00-05:00").expect("an offset form parses");
    assert_ne!(offset.offset(), time::UtcOffset::UTC);
    assert_eq!(format_rfc3339(offset), "2026-03-01T10:00:00.000000Z");

    assert!(parse_rfc3339("not an instant").is_err());
    assert!(
        parse_rfc3339("2025-09-04T15:33:20").is_err(),
        "an instant with no offset at all is not RFC 3339"
    );

    // **The parser is wider than the renderer, and measured rather than
    // assumed.** RFC 3339 section 5.6 permits a space in place of `T`, and
    // `time` honours that - so this reads back an instant the gear would never
    // have written. Asserted in the direction the parser actually goes, because
    // a round-trip test alone would never meet the form and a reader would take
    // the renderer's single spelling for the accepted set.
    let spaced = parse_rfc3339("2025-09-04 15:33:20Z").expect("a space separator is RFC 3339");
    assert_eq!(format_rfc3339(spaced), "2025-09-04T15:33:20.000000Z");
}

/// The other half of the option form: a field that is *present* goes through
/// the same normalisation as the mandatory one — parsed at its written offset,
/// carried as UTC, and rendered back in the gear's own six-digit spelling. The
/// absent case above exercises neither arm of that.
#[test]
fn a_present_optional_instant_is_normalised_and_rendered_like_the_mandatory_one() {
    let carrier: Carrier = serde_json::from_str(
        r#"{"at":"2026-01-01T00:00:00Z","maybe":"2026-03-01T12:00:00+02:00"}"#,
    )
    .expect("both fields parse");
    // Read back as UTC, not as the +02:00 the request wrote.
    assert_eq!(
        carrier.maybe,
        Some(at(
            1_772_359_200, // 2026-03-01T10:00:00Z
            0
        ))
    );
    assert_eq!(
        serde_json::to_string(&carrier).expect("serializes"),
        r#"{"at":"2026-01-01T00:00:00.000000Z","maybe":"2026-03-01T10:00:00.000000Z"}"#
    );
}

/// `#[serde(default)]` is scoped to an *absent* key. The contrast is the
/// claim: omit `maybe` and it reads as `None`, but write garbage into it and
/// the request is refused rather than quietly defaulting to `None` — which
/// would drop a date the caller believed it had sent.
#[test]
fn the_option_default_covers_an_absent_key_and_not_a_malformed_one() {
    let absent: Carrier =
        serde_json::from_str(r#"{"at":"2026-01-01T00:00:00Z"}"#).expect("absent key defaults");
    assert_eq!(absent.maybe, None);

    serde_json::from_str::<Carrier>(
        r#"{"at":"2026-01-01T00:00:00Z","maybe":"the first of March"}"#,
    )
    .expect_err("a malformed present key is refused, not defaulted");
}
