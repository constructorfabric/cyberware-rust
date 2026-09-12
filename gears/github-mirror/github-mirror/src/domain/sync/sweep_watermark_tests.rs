use chrono::{DateTime, Duration, Utc};

use super::{SWEEP_OVERLAP, high_water, stop_threshold};
use crate::domain::repo::SyncWatermarkRecord;

fn stored(last_seen: Option<&str>) -> SyncWatermarkRecord {
    SyncWatermarkRecord {
        repo_id: 1,
        family: "issues".to_owned(),
        last_seen_updated_at: last_seen.map(ToOwned::to_owned),
        page1_etag: None,
        sweep_in_progress: false,
        candidate_high_water: None,
    }
}

fn at(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .expect("test timestamps must parse")
        .with_timezone(&Utc)
}

#[test]
fn a_first_sweep_has_no_lower_bound() {
    assert_eq!(stop_threshold(None, false), None);
}

#[test]
fn force_ignores_the_stored_watermark() {
    let row = stored(Some("2026-09-01T10:00:00Z"));
    assert_eq!(stop_threshold(Some(&row), true), None);
}

#[test]
fn the_threshold_steps_back_by_the_overlap() {
    let row = stored(Some("2026-09-01T10:00:00Z"));
    assert_eq!(
        stop_threshold(Some(&row), false),
        Some(at("2026-09-01T10:00:00Z") - SWEEP_OVERLAP)
    );
}

#[test]
fn an_unparseable_watermark_falls_back_to_a_full_sweep() {
    let row = stored(Some("not a timestamp"));
    assert_eq!(stop_threshold(Some(&row), false), None);
}

#[test]
fn the_high_water_is_the_newest_entity_seen() {
    let seen = ["2026-09-01T10:00:00Z", "2026-09-03T08:00:00Z"];
    assert_eq!(high_water(&seen, None), Some(at("2026-09-03T08:00:00Z")));
}

#[test]
fn an_empty_sweep_keeps_the_threshold_it_started_from() {
    let threshold = at("2026-09-01T10:00:00Z");
    assert_eq!(high_water(&[], Some(threshold)), Some(threshold));
}

#[test]
fn the_high_water_never_moves_backwards() {
    let threshold = at("2026-09-03T00:00:00Z");
    let seen = ["2026-09-01T10:00:00Z"];
    assert_eq!(
        high_water(&seen, Some(threshold)),
        Some(threshold),
        "an older entity must not pull the watermark back"
    );
}

#[test]
fn the_overlap_is_wide_enough_for_clock_skew() {
    assert!(SWEEP_OVERLAP >= Duration::minutes(1));
}
