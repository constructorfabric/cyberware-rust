//! Tests for the future-publication-time bound. The interesting cases are at the
//! edge: the guard has to let a whole day of legitimate timezone/clock spread
//! through, so the accept and reject cases are paired to pin the real value.

use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use super::{MAX_FUTURE_SKEW_HOURS, reject_future_publication_time};

const PROVIDER: &str = "ecb";

/// A fixed "now" — no wall-clock reads, so the assertions cannot drift.
fn now() -> OffsetDateTime {
    OffsetDateTime::parse("2026-07-30T12:00:00Z", &Rfc3339).unwrap()
}

#[test]
fn a_past_publication_time_is_accepted() {
    // The normal case: ECB dates the document earlier the same day.
    let as_of = now() - Duration::hours(12);
    assert!(reject_future_publication_time(as_of, now(), PROVIDER).is_ok());
}

#[test]
fn a_very_old_publication_time_is_still_accepted_here() {
    // Age is the LEDGER's staleness rule, not this guard's. Rejecting an old
    // document here would silently duplicate that policy in the wrong layer.
    let as_of = now() - Duration::days(400);
    assert!(reject_future_publication_time(as_of, now(), PROVIDER).is_ok());
}

#[test]
fn a_publication_time_exactly_at_the_bound_is_accepted() {
    let as_of = now() + Duration::hours(MAX_FUTURE_SKEW_HOURS);
    assert!(
        reject_future_publication_time(as_of, now(), PROVIDER).is_ok(),
        "the bound is inclusive; a document exactly at it must still serve"
    );
}

#[test]
fn one_second_past_the_bound_is_rejected() {
    let as_of = now() + Duration::hours(MAX_FUTURE_SKEW_HOURS) + Duration::seconds(1);
    assert!(reject_future_publication_time(as_of, now(), PROVIDER).is_err());
}

#[test]
fn a_day_ahead_is_accepted_so_a_date_only_feed_never_breaks() {
    // A date-only feed anchored at 00:00:00 UTC can legitimately read a full day
    // ahead of a host whose clock sits just before the rollover.
    let as_of = now() + Duration::hours(24);
    assert!(reject_future_publication_time(as_of, now(), PROVIDER).is_ok());
}

#[test]
fn a_far_future_document_is_rejected_without_echoing_anything_but_timestamps() {
    let as_of = now() + Duration::days(30);
    let err = reject_future_publication_time(as_of, now(), PROVIDER).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("2026-08-29"),
        "the rejection must name the offending timestamp: {msg}"
    );
}
