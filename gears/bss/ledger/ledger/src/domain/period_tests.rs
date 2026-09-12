//! Tests for the pure period-id math.

use super::*;
use crate::domain::instant::utc_ymd_hms;

#[test]
fn next_period_id_advances_within_year() {
    assert_eq!(next_period_id("202606"), Some("202607".to_owned()));
}

#[test]
fn next_period_id_rolls_december_into_next_january() {
    assert_eq!(next_period_id("202612"), Some("202701".to_owned()));
}

#[test]
fn next_period_id_rejects_short_input() {
    assert_eq!(next_period_id("2026"), None);
}

#[test]
fn next_period_id_rejects_out_of_range_month() {
    assert_eq!(next_period_id("202613"), None);
    assert_eq!(next_period_id("202600"), None);
}

#[test]
fn period_id_for_formats_year_month() {
    let now = utc_ymd_hms(2026, 6, 19, 0, 0, 0);
    assert_eq!(period_id_for(now), "202606");
}

#[test]
fn period_id_plus_zero_is_identity() {
    assert_eq!(period_id_plus("202606", 0), Some("202606".to_owned()));
}

#[test]
fn period_id_plus_one_matches_next_period_id() {
    assert_eq!(
        period_id_plus("202611", 1),
        next_period_id("202611"),
        "period_id_plus(_, 1) must agree with next_period_id"
    );
    assert_eq!(period_id_plus("202611", 1), Some("202612".to_owned()));
}

#[test]
fn period_id_plus_crosses_year_boundaries() {
    // 2026-06 + 12 months → 2027-06 (full year).
    assert_eq!(period_id_plus("202606", 12), Some("202706".to_owned()));
    // 2026-11 + 3 → 2027-02 (rolls the year once).
    assert_eq!(period_id_plus("202611", 3), Some("202702".to_owned()));
    // 2026-12 + 1 → 2027-01 (December roll, matches next_period_id).
    assert_eq!(period_id_plus("202612", 1), Some("202701".to_owned()));
}

#[test]
fn period_id_plus_rejects_malformed_input() {
    assert_eq!(period_id_plus("2026", 1), None);
    assert_eq!(period_id_plus("202613", 1), None);
    assert_eq!(period_id_plus("202600", 1), None);
}

#[test]
fn previous_period_id_decrements_within_year() {
    assert_eq!(previous_period_id("202606"), Some("202605".to_owned()));
}

#[test]
fn previous_period_id_rolls_january_into_prior_december() {
    assert_eq!(previous_period_id("202601"), Some("202512".to_owned()));
}

#[test]
fn previous_period_id_is_inverse_of_next() {
    assert_eq!(
        previous_period_id(&next_period_id("202606").unwrap()),
        Some("202606".to_owned())
    );
    assert_eq!(previous_period_id("2026"), None);
    assert_eq!(previous_period_id("202600"), None);
}

#[test]
fn period_start_utc_is_first_instant_of_month() {
    assert_eq!(
        period_start_utc("202606"),
        Some(utc_ymd_hms(2026, 6, 1, 0, 0, 0))
    );
    assert_eq!(period_start_utc("2026"), None);
    assert_eq!(period_start_utc("202613"), None);
}

#[test]
fn period_end_utc_is_first_instant_of_next_month() {
    // End of 2026-06 is the first instant of 2026-07.
    assert_eq!(
        period_end_utc("202606"),
        Some(utc_ymd_hms(2026, 7, 1, 0, 0, 0))
    );
    // December rolls into next January.
    assert_eq!(
        period_end_utc("202612"),
        Some(utc_ymd_hms(2027, 1, 1, 0, 0, 0))
    );
    assert_eq!(period_end_utc("bad"), None);
}
