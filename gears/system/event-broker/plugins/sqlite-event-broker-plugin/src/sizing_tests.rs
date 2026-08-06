//! The stored-size rule: what it counts, and what it refuses to count as zero.

use chrono::Utc;
use event_broker_sdk::models::Event;
use uuid::Uuid;

use super::{FIXED_ROW_BYTES, stored_bytes};

fn event_with_payload(payload: serde_json::Value) -> Event {
    Event {
        id: Uuid::nil(),
        type_id: String::new(),
        tenant_id: Uuid::nil(),
        source: String::new(),
        subject: String::new(),
        subject_type: String::new(),
        occurred_at: Utc::now(),
        trace_parent: None,
        data: Some(payload),
        partition: None,
        sequence: None,
        sequence_time: None,
        meta: None,
    }
}

#[test]
fn an_event_with_no_text_at_all_still_costs_its_rows_fixed_overhead() {
    let mut event = event_with_payload(serde_json::Value::Null);
    event.data = None;
    assert_eq!(
        stored_bytes(&event),
        FIXED_ROW_BYTES,
        "an empty event accounted as zero bytes would let unlimited numbers of \
         them accumulate under any byte bound"
    );
}

#[test]
fn the_payload_and_every_text_column_count_toward_the_figure() {
    let mut event = event_with_payload(serde_json::json!({ "k": "v" }));
    event.type_id = "gts.x.eb.t1.foo.v1~".to_owned();
    event.source = "svc".to_owned();

    // `{"k":"v"}` is nine bytes serialized.
    let expected = FIXED_ROW_BYTES + 19 + 3 + 9;
    assert_eq!(stored_bytes(&event), expected);
}

#[test]
fn a_larger_payload_costs_strictly_more() {
    let small = stored_bytes(&event_with_payload(serde_json::json!({ "k": "v" })));
    let large = stored_bytes(&event_with_payload(
        serde_json::json!({ "k": "v".repeat(1000) }),
    ));
    assert!(
        large > small,
        "size accounting must track what grows: {large} was not greater than {small}"
    );
}
