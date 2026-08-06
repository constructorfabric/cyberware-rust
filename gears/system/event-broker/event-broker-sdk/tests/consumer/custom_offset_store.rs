use event_broker_sdk::Sequence;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use event_broker_sdk::{
    CommitOffset, ConsumerBuilder, ConsumerCommitMode, ConsumerError, ConsumerGroupId,
    ConsumerGroupRef, Fallback, HandlerOutcome, OffsetManagerError, OffsetStore, Position,
    RawEvent, SingleEventHandler, TopicId,
};

use super::common::{publish_json, topic_fixture, wait_until};

const TOPIC: &str = "gts.cf.core.events.topic.v1~example.mock.showcase.customoffset.v1";
const EVENT_TYPE: &str = "gts.cf.core.events.event.v1~example.mock.showcase.customoffset.v1~";

type LoadCall = (ConsumerGroupId, TopicId, u32);
type CommitCall = (ConsumerGroupId, TopicId, u32, i64);
type RecordedLoads = Arc<Mutex<Vec<LoadCall>>>;
type RecordedCommits = Arc<Mutex<Vec<CommitCall>>>;

#[derive(Clone)]
struct RecordingOffsetStore {
    loads: RecordedLoads,
    commits: RecordedCommits,
}

#[async_trait]
impl OffsetStore for RecordingOffsetStore {
    async fn load_position(
        &self,
        group: &ConsumerGroupId,
        topic: &TopicId,
        partition: u32,
    ) -> Result<Position, OffsetManagerError> {
        self.loads.lock().unwrap().push((*group, *topic, partition));
        Ok(Fallback::Earliest.into())
    }
}

#[async_trait]
impl CommitOffset for RecordingOffsetStore {
    async fn commit(
        &self,
        group: &ConsumerGroupId,
        topic: &TopicId,
        partition: u32,
        offset: Sequence,
    ) -> Result<(), OffsetManagerError> {
        self.commits
            .lock()
            .unwrap()
            .push((*group, *topic, partition, offset.as_i64()));
        Ok(())
    }
}

struct AckingHandler;

#[async_trait]
impl SingleEventHandler for AckingHandler {
    async fn handle(
        &self,
        _event: RawEvent,
        _attempts: u16,
    ) -> Result<HandlerOutcome, ConsumerError> {
        Ok(HandlerOutcome::Success)
    }
}

#[tokio::test]
async fn if_i_want_my_own_offset_store_i_implement_the_minimal_traits() {
    let fixture = topic_fixture(TOPIC, EVENT_TYPE, 1).await;
    let store = RecordingOffsetStore {
        loads: Arc::new(Mutex::new(Vec::new())),
        commits: Arc::new(Mutex::new(Vec::new())),
    };
    let commits = store.commits.clone();

    let handle = ConsumerBuilder::new(fixture.broker.clone())
        .group(ConsumerGroupRef::auto_anonymous("showcase-custom-offset"))
        .topics([TOPIC])
        .commit_mode(ConsumerCommitMode::auto(Duration::from_millis(5)))
        .offset_manager(store)
        .handler(AckingHandler)
        .start()
        .await
        .expect("consumer starts");

    publish_json(
        &fixture.broker,
        &fixture.ctx,
        EVENT_TYPE,
        "custom-offset-1",
        None,
        serde_json::json!({ "custom": true }),
    )
    .await;

    wait_until(|| commits.lock().unwrap().iter().any(|commit| commit.3 >= 0)).await;
    handle.stop().await.expect("consumer stops");

    let committed = commits.lock().unwrap();
    assert_eq!(committed[0].2, 0);
    assert!(committed.iter().any(|commit| commit.3 >= 0));
}
