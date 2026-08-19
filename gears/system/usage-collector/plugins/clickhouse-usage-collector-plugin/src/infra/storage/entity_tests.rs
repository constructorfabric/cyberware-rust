use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use usage_collector_sdk::UsageKind;
use uuid::Uuid;

use super::{UsageRecordStatusCode, UsageTypeKindCode, ch_decimal128_9, ch_uuid, ch_uuid_opt};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct UuidWrap(#[serde(with = "ch_uuid")] Uuid);

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct UuidOptWrap(#[serde(with = "ch_uuid_opt")] Option<Uuid>);

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct DecimalWrap(#[serde(with = "ch_decimal128_9")] Decimal);

#[test]
fn ch_uuid_json_round_trip() {
    let u = Uuid::from_u128(0xabcd_ef01);
    let json = serde_json::to_string(&UuidWrap(u)).unwrap();
    let back: UuidWrap = serde_json::from_str(&json).unwrap();
    assert_eq!(back.0, u);
}

#[test]
fn ch_uuid_binary_round_trip() {
    let u = Uuid::from_u128(0x1234_5678_9abc);
    let bytes = postcard::to_allocvec(&UuidWrap(u)).unwrap();
    let back: UuidWrap = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(back.0, u);
}

#[test]
fn ch_uuid_opt_json_round_trips_some_and_none() {
    let some = UuidOptWrap(Some(Uuid::from_u128(7)));
    let none = UuidOptWrap(None);

    let some_json = serde_json::to_string(&some).unwrap();
    let none_json = serde_json::to_string(&none).unwrap();
    assert_eq!(
        serde_json::from_str::<UuidOptWrap>(&some_json).unwrap(),
        some
    );
    assert_eq!(
        serde_json::from_str::<UuidOptWrap>(&none_json).unwrap(),
        none
    );
}

#[test]
fn ch_uuid_opt_binary_round_trips_some_and_none() {
    let some = UuidOptWrap(Some(Uuid::from_u128(9)));
    let none = UuidOptWrap(None);

    let some_bytes = postcard::to_allocvec(&some).unwrap();
    let none_bytes = postcard::to_allocvec(&none).unwrap();
    assert_eq!(
        postcard::from_bytes::<UuidOptWrap>(&some_bytes).unwrap(),
        some
    );
    assert_eq!(
        postcard::from_bytes::<UuidOptWrap>(&none_bytes).unwrap(),
        none
    );
}

#[test]
fn ch_decimal128_9_json_round_trip() {
    let d = Decimal::new(425, 1); // 42.5
    let json = serde_json::to_string(&DecimalWrap(d)).unwrap();
    let back: DecimalWrap = serde_json::from_str(&json).unwrap();
    assert_eq!(back.0, d);
}

#[test]
fn ch_decimal128_9_binary_round_trip() {
    let d = Decimal::new(-1_250_000_009, 9);
    let bytes = postcard::to_allocvec(&DecimalWrap(d)).unwrap();
    let back: DecimalWrap = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(back.0, d);
}

#[test]
fn ch_decimal128_9_binary_rejects_overflow_mantissa() {
    // Construct an i128 mantissa that needs more than 96 bits when interpreted
    // at scale 9 — the deserialize guard must fail closed.
    let too_wide: i128 = 1_i128 << 100;
    let bytes = postcard::to_allocvec(&too_wide).unwrap();
    assert!(
        postcard::from_bytes::<DecimalWrap>(&bytes).is_err(),
        "mantissa wider than 96 bits must fail Decimal128(9) decode"
    );
}

#[test]
fn ch_decimal128_9_binary_rejects_rescale_that_cannot_reach_scale_9() {
    // Near-max mantissa at scale 0 cannot be multiplied by 10^9 without
    // overflowing rust_decimal's 96-bit capacity, so `rescale(9)` stops short.
    // Binary serialize must fail closed rather than emit a wrong-scale mantissa.
    let too_large = Decimal::from_parts(u32::MAX, u32::MAX, u32::MAX, false, 0);
    // Human-readable path is unchanged and must still succeed for the same value.
    assert!(serde_json::to_string(&DecimalWrap(too_large)).is_ok());
    // postcard erases the serde custom message to `SerdeSerCustom`; asserting
    // failure is enough — successful binary round-trips are covered above.
    assert!(
        postcard::to_allocvec(&DecimalWrap(too_large)).is_err(),
        "mantissa that cannot reach scale 9 must fail Decimal128(9) encode"
    );
}

#[test]
fn ch_uuid_json_rejects_malformed() {
    let err = serde_json::from_str::<UuidWrap>("\"not-a-uuid\"").unwrap_err();
    assert!(!err.to_string().is_empty());
}

#[test]
fn usage_kind_round_trips_through_code_form() {
    // `UsageTypeKindCode` is a closed `#[repr(i8)]` enum, so there is no
    // "rejects unknown" case to test — only the From/Into round-trip.
    for kind in [UsageKind::Counter, UsageKind::Gauge] {
        let code = UsageTypeKindCode::from(kind);
        assert_eq!(UsageKind::from(code), kind);
    }
}

#[test]
fn usage_record_status_round_trips_through_code_form() {
    use usage_collector_sdk::UsageRecordStatus;

    for status in [UsageRecordStatus::Active, UsageRecordStatus::Inactive] {
        let code = UsageRecordStatusCode::from(status);
        assert_eq!(UsageRecordStatus::from(code), status);
    }
}
