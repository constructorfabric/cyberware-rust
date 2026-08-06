//! Tests for the ordinal vocabulary.
//!
//! What a sequence must *not* do is asserted by a compile-fail case rather
//! than here - see `tests/trybuild/sequence/no_arithmetic.rs`, driven from
//! `tests/sdk/sequence.rs`. A runtime test cannot prove the absence of an
//! operator; only the compiler can.

use crate::sequence::Sequence;

#[test]
fn ordering_is_the_operation_the_type_keeps() {
    let earlier = Sequence::assigned(6);
    let later = Sequence::assigned(50);

    assert!(earlier < later);
    assert!(later > earlier);
    assert_eq!(earlier.min(later), earlier);
    assert_eq!(earlier.max(later), later);
}

#[test]
fn equality_and_hashing_work_so_a_sequence_can_key_a_map() {
    use std::collections::HashMap;

    let mut seen: HashMap<Sequence, &str> = HashMap::new();
    seen.insert(Sequence::assigned(7), "seven");

    assert_eq!(seen.get(&Sequence::assigned(7)), Some(&"seven"));
    assert_eq!(seen.get(&Sequence::assigned(8)), None);
}

#[test]
fn ordering_holds_across_a_gap() {
    // The space is sparse: 7 and 50 may be adjacent in storage with nothing
    // between them. Comparison is indifferent to that, which is why comparison
    // is safe and subtraction is not.
    let before = Sequence::assigned(7);
    let after = Sequence::assigned(50);

    assert!(before < after);
}

#[test]
fn none_is_zero_and_is_never_an_event_position() {
    assert_eq!(Sequence::NONE.as_i64(), 0);
    assert!(Sequence::NONE.is_none());
    // Sequences an event can hold start at 1, so NONE sorts below every one.
    assert!(Sequence::NONE < Sequence::assigned(1));
    assert!(!Sequence::assigned(1).is_none());
}

#[test]
fn default_is_no_position() {
    assert_eq!(Sequence::default(), Sequence::NONE);
}

#[test]
fn the_storage_boundary_round_trips_explicitly() {
    let stored = 4_294_967_296_i64;
    assert_eq!(Sequence::assigned(stored).as_i64(), stored);
}

#[test]
fn serde_renders_a_bare_integer() {
    // Transparent on the wire: the type is a compile-time distinction, not a
    // change to what a consumer receives.
    let json = serde_json::to_string(&Sequence::assigned(42)).expect("serializes");
    assert_eq!(json, "42");

    let parsed: Sequence = serde_json::from_str("42").expect("deserializes");
    assert_eq!(parsed, Sequence::assigned(42));
}

#[test]
fn serde_round_trips_inside_an_option() {
    // `Event::sequence` is `Option<Sequence>`: absent on a publish payload,
    // present once storage has assigned it.
    let absent: Option<Sequence> = None;
    assert_eq!(serde_json::to_string(&absent).expect("serializes"), "null");

    let present: Option<Sequence> = serde_json::from_str("7").expect("deserializes");
    assert_eq!(present, Some(Sequence::assigned(7)));
}

#[test]
fn display_is_the_bare_number() {
    // Error messages quote positions, so the rendering must not leak the
    // wrapper.
    assert_eq!(Sequence::assigned(5).to_string(), "5");
    assert_eq!(Sequence::NONE.to_string(), "0");
}
