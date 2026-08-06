use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use event_broker_sdk::{
    BatchHandlerOutcome, ConsumerBatching, ConsumerBuilder, ConsumerError, ConsumerGroupRef,
    ConsumerHandler, EventBatch, Fallback, InMemoryOffsetManager,
};

use super::common::{publish_json, topic_fixture, wait_until};

const TOPIC: &str = "gts.cf.core.events.topic.v1~example.mock.showcase.batch.v1";
const EVENT_TYPE: &str = "gts.cf.core.events.event.v1~example.mock.showcase.batch.v1~";

type RecordedBatch = (u32, Vec<i64>);
type RecordedBatches = Arc<Mutex<Vec<RecordedBatch>>>;

struct BatchProjector {
    batches: RecordedBatches,
}

#[async_trait]
impl ConsumerHandler for BatchProjector {
    async fn handle_batch(
        &self,
        batch: &EventBatch<'_>,
        _attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError> {
        let chunk = batch.next_chunk(batch.len());
        self.batches.lock().unwrap().push((
            chunk[0].partition,
            chunk.iter().map(|event| event.offset.as_i64()).collect(),
        ));
        Ok(BatchHandlerOutcome::AdvanceThrough {
            offset: chunk.last().expect("showcase batch is not empty").offset,
        })
    }
}

#[tokio::test]
async fn if_i_want_native_batches_they_stay_inside_one_partition() {
    let fixture = topic_fixture(TOPIC, EVENT_TYPE, 2).await;
    let batches = Arc::new(Mutex::new(Vec::new()));

    let handle = ConsumerBuilder::new(fixture.broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("showcase-batch"))
        .topics([TOPIC])
        .batching(ConsumerBatching {
            max_events: 8,
            max_wait: Duration::from_millis(10),
        })
        .offset_manager(InMemoryOffsetManager::new(Fallback::Earliest))
        .batch_handler(BatchProjector {
            batches: batches.clone(),
        })
        .start()
        .await
        .expect("consumer starts");

    for partition in [0, 1] {
        publish_json(
            &fixture.broker,
            &fixture.ctx,
            EVENT_TYPE,
            &format!("batch-{partition}"),
            Some(partition),
            serde_json::json!({ "partition": partition }),
        )
        .await;
    }

    wait_until(|| batches.lock().unwrap().len() >= 2).await;
    handle.stop().await.expect("consumer stops");

    assert!(
        batches
            .lock()
            .unwrap()
            .iter()
            .all(|(_, offsets)| !offsets.is_empty())
    );
}
