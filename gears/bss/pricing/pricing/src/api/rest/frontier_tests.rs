//! Tests for the frontier DTO's two readings.
//!
//! The router-level behaviour (401, 403, and the happy path over a real
//! database) is `tests/rest_frontier.rs`; what is worth pinning here is the
//! discrimination the wire shape has to carry.

use super::PinFrontierView;
use crate::domain::instant::utc_ymd_hms;
use bss_pricing_sdk::{CatalogVersion, PinFrontier};

#[test]
fn no_frontier_yet_is_distinguishable_from_a_frontier_at_version_zero() {
    let nothing = PinFrontierView::none_yet();
    let at_zero = PinFrontierView::from(PinFrontier {
        catalog_version: CatalogVersion::new(0),
        advanced_at: utc_ymd_hms(2026, 8, 1, 12, 0, 0),
    });

    // The whole reason this is a 200 and not a 404: a consumer reading version
    // 0 has something it may pin, and a consumer reading `pin_eligible: false`
    // has not. Collapsing them would let a run resolve against a version no
    // publish ever produced.
    assert!(!nothing.pin_eligible);
    assert_eq!(nothing.catalog_version, None);
    assert_eq!(nothing.advanced_at, None);

    assert!(at_zero.pin_eligible);
    assert_eq!(at_zero.catalog_version, Some(0));
    assert!(at_zero.advanced_at.is_some());
}

#[test]
fn a_present_frontier_carries_the_version_and_the_instant_verbatim() {
    let advanced_at = utc_ymd_hms(2026, 8, 1, 9, 30, 0);
    let view = PinFrontierView::from(PinFrontier {
        catalog_version: CatalogVersion::new(42),
        advanced_at,
    });

    assert_eq!(view.catalog_version, Some(42));
    assert_eq!(view.advanced_at, Some(advanced_at));
}

#[test]
fn the_empty_reading_serializes_its_nulls_rather_than_omitting_them() {
    // A consumer discriminates on the fields being present and null. Omitting
    // them would make "no publish yet" indistinguishable from an older
    // serialization that never knew about the frontier at all.
    let json = serde_json::to_value(PinFrontierView::none_yet()).expect("serialize");

    let object = json.as_object().expect("the view serializes as an object");
    // The **names**, not the count. `len() == 3` is guaranteed by the struct plus
    // the macro's lack of `skip_serializing_if`, so it could not fail; a rename
    // passed it, and a rename is exactly what breaks the consumer this case is for.
    let mut members: Vec<&str> = object.keys().map(String::as_str).collect();
    members.sort_unstable();
    assert_eq!(
        members,
        ["advanced_at", "catalog_version", "pin_eligible"],
        "the three fields a consumer discriminates on: {json}"
    );
    assert!(object.values().any(serde_json::Value::is_null));
}
