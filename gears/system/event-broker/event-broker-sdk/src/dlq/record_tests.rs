use crate::sequence::Sequence;
use chrono::Utc;
use toolkit_gts::gts_id;
use uuid::Uuid;

use crate::consumer::RawEvent;
use crate::ids::{ConsumerGroupId, TopicId};

use super::DeadLetterRecord;

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
        partition: 3,
        sequence: Sequence::assigned(42),
        offset: Sequence::assigned(42),
        occurred_at: Utc::now(),
        sequence_time: Utc::now(),
        trace_parent: Some("00-test".to_owned()),
        data: serde_json::json!({ "order_id": "order-1" }),
    }
}

#[test]
fn dead_letter_record_exposes_context_fields_for_diagnosis_and_replay() {
    let group_id = ConsumerGroupId::new(Uuid::new_v4());
    let topic_id = TopicId::from_gts(TOPIC);
    let raw = raw_event();
    let payload = raw.data.clone();

    let record = DeadLetterRecord::builder(&raw, "schema mismatch")
        .group_id(group_id)
        .topic_id(topic_id)
        .attempts(2)
        .build();

    assert_eq!(record.group_id, Some(group_id));
    assert_eq!(record.topic_id, Some(topic_id));
    assert_eq!(record.topic, TOPIC);
    assert_eq!(record.event_type, EVENT_TYPE);
    assert_eq!(record.subject, "order-1");
    assert_eq!(record.subject_type, "order");
    assert_eq!(record.partition, 3);
    assert_eq!(record.offset, Sequence::assigned(42));
    assert_eq!(record.payload, payload);
    assert_eq!(record.reason, "schema mismatch");
    assert_eq!(record.attempts, Some(2));
    assert_eq!(record.event_id, raw.id);
}

#[test]
fn dead_letter_record_allows_absent_group_and_topic_id() {
    let raw = raw_event();

    let record = DeadLetterRecord::builder(&raw, "no registry context")
        .without_topic_id()
        .build();

    assert_eq!(record.group_id, None);
    assert_eq!(record.topic_id, None);
    assert_eq!(record.topic, TOPIC);
    assert_eq!(record.reason, "no registry context");
}
