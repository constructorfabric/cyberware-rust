use crate::sequence::Sequence;
use chrono::Utc;
use toolkit_gts::gts_id;
use uuid::Uuid;

use crate::consumer::RawEvent;
use crate::ids::{ConsumerGroupId, TopicId};

use super::{DeadLetterEnvelope, DeadLetterRecord};

const TOPIC: &str = gts_id!("cf.core.events.topic.v1~example.orders.x.x.v1");
const EVENT_TYPE: &str = gts_id!("cf.core.events.event.v1~example.orders.rejected.x.v1~");

fn raw_event() -> RawEvent {
    RawEvent {
        id: Uuid::new_v4(),
        type_id: EVENT_TYPE.to_owned(),
        topic: TOPIC.to_owned(),
        tenant_id: Uuid::new_v4(),
        subject: "order-1".to_owned(),
        subject_type: "order".to_owned(),
        partition: 7,
        sequence: Sequence::assigned(99),
        offset: Sequence::assigned(99),
        occurred_at: Utc::now(),
        sequence_time: Utc::now(),
        trace_parent: None,
        data: serde_json::json!({ "order_id": "order-1", "valid": false }),
    }
}

#[test]
fn dead_letter_envelope_preserves_record_context_and_payload_type_convention() {
    let group_id = ConsumerGroupId::new(Uuid::new_v4());
    let topic_id = TopicId::from_gts(TOPIC);
    let raw = raw_event();
    let record = DeadLetterRecord::builder(&raw, "permanent validation failure")
        .group_id(group_id)
        .topic_id(topic_id)
        .attempts(6)
        .build();

    let envelope = DeadLetterEnvelope::from_record(record);

    assert_eq!(DeadLetterEnvelope::VERSION, 1);
    assert_eq!(
        DeadLetterEnvelope::PAYLOAD_TYPE,
        "application/vnd.constructorfabric.event-broker.dlq+json"
    );
    assert_eq!(envelope.version, DeadLetterEnvelope::VERSION);
    assert_eq!(envelope.group_id, Some(group_id));
    assert_eq!(envelope.topic_id, Some(topic_id));
    assert_eq!(envelope.topic, TOPIC);
    assert_eq!(envelope.event_type, EVENT_TYPE);
    assert_eq!(envelope.subject, "order-1");
    assert_eq!(envelope.subject_type, "order");
    assert_eq!(envelope.partition, 7);
    assert_eq!(envelope.offset, Sequence::assigned(99));
    assert_eq!(envelope.attempts, Some(6));
    assert_eq!(envelope.reason, "permanent validation failure");
    assert_eq!(envelope.payload, raw.data);
    assert_eq!(envelope.event_id, raw.id);
}

#[test]
fn dead_letter_envelope_round_trips_from_outbox_payload() {
    let raw = raw_event();
    let record = DeadLetterRecord::builder(&raw, "cannot project").build();
    let envelope = DeadLetterEnvelope::from_record(record);

    let payload = envelope.to_vec().expect("serialize envelope");
    let decoded = DeadLetterEnvelope::from_slice(&payload).expect("deserialize envelope");

    assert_eq!(decoded, envelope);
    let coordinates = decoded.source_coordinates();
    assert_eq!(coordinates.topic, TOPIC);
    assert_eq!(coordinates.event_type, EVENT_TYPE);
    assert_eq!(coordinates.subject, "order-1");
    assert_eq!(coordinates.subject_type, "order");
    assert_eq!(coordinates.partition, 7);
    assert_eq!(coordinates.offset, Sequence::assigned(99));
    assert_eq!(coordinates.event_id, raw.id);
}

#[test]
fn invalid_dead_letter_envelope_payload_returns_consumer_error() {
    let err = DeadLetterEnvelope::from_slice(b"not json")
        .expect_err("invalid payload should not deserialize");

    assert!(err.to_string().contains("deserialize dead-letter envelope"));
}
