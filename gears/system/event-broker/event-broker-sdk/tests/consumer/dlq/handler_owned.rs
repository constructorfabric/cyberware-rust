use event_broker_sdk::Sequence;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use event_broker_sdk::dlq::{DeadLetterRecord, DeadLetterSink};
use event_broker_sdk::{
    ConsumerBuilder, ConsumerError, ConsumerGroupRef, EventBrokerError, Fallback, HandlerOutcome,
    InMemoryOffsetManager, RawEvent, SingleEventHandler,
};
use uuid::Uuid;

use crate::consumer::common::{publish_json, topic_fixture, wait_until};

const TOPIC: &str = "gts.cf.core.events.topic.v1~example.mock.showcase.dlq.v1";
const EVENT_TYPE: &str = "gts.cf.core.events.event.v1~example.mock.showcase.dlq.v1~";

#[derive(Clone, Default)]
struct RecordingDeadLetterSink {
    records: Arc<Mutex<Vec<DeadLetterRecord>>>,
    fail: bool,
}

fn rejected_event() -> RawEvent {
    RawEvent {
        id: Uuid::new_v4(),
        type_id: EVENT_TYPE.to_owned(),
        topic: TOPIC.to_owned(),
        tenant_id: Uuid::nil(),
        subject: "dlq-1".to_owned(),
        subject_type: "test".to_owned(),
        partition: 0,
        sequence: Sequence::assigned(1),
        offset: Sequence::assigned(1),
        occurred_at: Utc::now(),
        sequence_time: Utc::now(),
        trace_parent: None,
        data: serde_json::json!({ "reject": true }),
    }
}

#[async_trait]
impl DeadLetterSink for RecordingDeadLetterSink {
    async fn park(&self, record: DeadLetterRecord) -> Result<(), ConsumerError> {
        if self.fail {
            return Err(EventBrokerError::Internal(
                "dead-letter sink unavailable".to_owned(),
            ));
        }
        self.records.lock().unwrap().push(record);
        Ok(())
    }
}

struct HandlerOwnedDlqPolicy {
    sink: RecordingDeadLetterSink,
}

impl HandlerOwnedDlqPolicy {
    async fn park_permanent_failure(
        &self,
        event: &RawEvent,
        attempts: u16,
    ) -> Result<(), ConsumerError> {
        let record = DeadLetterRecord::builder(event, "showcase reject")
            .attempts(attempts)
            .build();
        self.sink.park(record).await
    }
}

#[async_trait]
impl SingleEventHandler for HandlerOwnedDlqPolicy {
    async fn handle(
        &self,
        event: RawEvent,
        attempts: u16,
    ) -> Result<HandlerOutcome, ConsumerError> {
        if event.data.get("reject").and_then(|value| value.as_bool()) == Some(true) {
            self.park_permanent_failure(&event, attempts).await?;
            return Ok(HandlerOutcome::Success);
        }

        Ok(HandlerOutcome::Success)
    }
}

#[tokio::test]
async fn if_i_want_permanent_failures_parked_i_do_it_in_the_handler() {
    let fixture = topic_fixture(TOPIC, EVENT_TYPE, 1).await;
    let sink = RecordingDeadLetterSink::default();
    let records = sink.records.clone();

    let handle = ConsumerBuilder::new(fixture.broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("showcase-dlq"))
        .topics([TOPIC])
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .handler(HandlerOwnedDlqPolicy { sink })
        .start()
        .await
        .expect("consumer starts");

    publish_json(
        &fixture.broker,
        &fixture.ctx,
        EVENT_TYPE,
        "dlq-1",
        None,
        serde_json::json!({ "reject": true }),
    )
    .await;

    wait_until(|| records.lock().unwrap().len() == 1).await;
    handle.stop().await.expect("consumer stops");

    let records = records.lock().unwrap();
    assert_eq!(records[0].reason, "showcase reject");
    assert_eq!(records[0].payload, serde_json::json!({ "reject": true }));
    assert_eq!(records[0].topic, TOPIC);
}

#[tokio::test]
async fn if_my_dead_letter_sink_fails_i_return_an_error_and_the_source_offset_is_not_successful() {
    let sink = RecordingDeadLetterSink {
        fail: true,
        ..RecordingDeadLetterSink::default()
    };
    let records = sink.records.clone();
    let handler = HandlerOwnedDlqPolicy { sink };

    let err = handler
        .park_permanent_failure(&rejected_event(), 6)
        .await
        .expect_err("failed parking must keep the handler from returning success");

    assert!(err.to_string().contains("dead-letter sink unavailable"));
    assert!(records.lock().unwrap().is_empty());
}
