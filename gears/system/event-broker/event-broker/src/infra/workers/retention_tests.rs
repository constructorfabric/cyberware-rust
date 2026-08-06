//! The retention worker: what it drives, how often, and what it skips.
//!
//! Every sweep here is called directly. Nothing sleeps and nothing waits on a
//! spawned task, so "three passes happened" is a fact rather than a hope.

use crate::domain::model::Sequence;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use event_broker_sdk::models::{Event as SdkEvent, PartitionLeader, PartitionRange, TopicSegment};
use event_broker_sdk::{
    EventBrokerBackend, RetentionReport, RetentionRequest, StorageBackendError,
};
use serde_json::json;
use toolkit_gts::GtsInstanceId;
use toolkit_security::SecurityContext;

use gts::GtsTypeId;

use crate::config::EventBrokerConfig;
use crate::domain::backend::BackendResolver;
use crate::domain::error::DomainError;
use crate::domain::resolution::{EffectiveSettings, Sourced};
use event_broker_sdk::models::EventType;

use crate::domain::model::Topic;
use crate::domain::specification::SpecificationManager;

use super::RetentionWorker;

const TOPIC: &str = "gts.cf.core.events.topic.v1~x.eb.t1.topic.v1";
const TOPIC_TYPE: &str = "gts.cf.core.events.topic.v1~";
const OTHER_TOPIC: &str = "gts.cf.core.events.subscription.v1~x.eb.t1.other.v1";

fn id(raw: &str) -> GtsInstanceId {
    GtsInstanceId::try_new(raw).expect("test id is a valid GTS instance id")
}

/// A topic as the cache holds it: the projection, plus the settings a
/// deployment resolved. `partitions` is what those settings say, not something
/// the topic carries.
fn topic(raw: &str, partitions: i32) -> Topic {
    Topic {
        id: id(raw),
        description: "a topic under retention".to_owned(),
        retention: None,
        settings: EffectiveSettings::builder(Sourced::configured_for_topic(partitions))
            .build(&config(&json!({}))),
    }
}

/// A `SpecificationManager` that answers `list_topics` and nothing else, which
/// is all the worker asks of one.
struct StubSpecs {
    topics: Vec<Topic>,
}

#[async_trait]
impl SpecificationManager for StubSpecs {
    async fn get_topic(&self, id: &GtsInstanceId) -> Option<Topic> {
        self.topics.iter().find(|t| t.id == *id).cloned()
    }
    async fn get_event_type(&self, _id: &GtsTypeId) -> Option<EventType> {
        None
    }
    async fn validate_event_data(
        &self,
        _event_type: &EventType,
        _data: &serde_json::Value,
    ) -> Result<(), DomainError> {
        Ok(())
    }
    async fn list_topics(&self) -> Vec<Topic> {
        self.topics.clone()
    }
    async fn list_event_types(&self) -> Vec<EventType> {
        Vec::new()
    }
    async fn resolve_topic_id(&self, _id: &GtsInstanceId) -> Result<i64, DomainError> {
        Ok(1)
    }
    async fn resolve_event_type_id(&self, _id: &GtsTypeId) -> Result<i64, DomainError> {
        Ok(1)
    }
}

/// One recorded pass: which topic, which partition, and the byte bound it
/// carried, if any.
type RecordedPass = (String, u32, Option<u64>);

/// A backend that records the retention requests it was handed and removes
/// nothing. What is under test here is what the worker drives, not what a
/// backend does with it - the removal itself has its own tests, in the crate
/// that performs it.
#[derive(Default)]
struct RecordingBackend {
    passes: std::sync::Mutex<Vec<RecordedPass>>,
    failures: AtomicU64,
    fail_every_pass: bool,
}

#[async_trait]
impl EventBrokerBackend for RecordingBackend {
    async fn persist(
        &self,
        _ctx: &SecurityContext,
        _topic: &str,
        _partition: u32,
        _events: &[SdkEvent],
    ) -> Result<(), StorageBackendError> {
        Ok(())
    }
    async fn resolve(
        &self,
        _ctx: &SecurityContext,
        _topic: &str,
        _partition: u32,
        _position: event_broker_sdk::api::Position,
    ) -> Result<Sequence, StorageBackendError> {
        Ok(Sequence::NONE)
    }

    async fn read(
        &self,
        _ctx: &SecurityContext,
        _topic: &str,
        _partition: u32,
        _after: Sequence,
        _max_count: usize,
    ) -> Result<Vec<SdkEvent>, StorageBackendError> {
        Ok(Vec::new())
    }
    async fn query(
        &self,
        _ctx: &SecurityContext,
        _topic: &str,
        _partition: u32,
        _range: PartitionRange,
    ) -> Result<Vec<TopicSegment>, StorageBackendError> {
        Ok(Vec::new())
    }
    async fn list_partition_leaders(
        &self,
        _ctx: &SecurityContext,
        _topic: &str,
    ) -> Result<Vec<PartitionLeader>, StorageBackendError> {
        Ok(Vec::new())
    }
    async fn maintain(
        &self,
        _ctx: &SecurityContext,
        request: &RetentionRequest,
    ) -> Result<RetentionReport, StorageBackendError> {
        if self.fail_every_pass {
            self.failures.fetch_add(1, Ordering::Relaxed);
            return Err(StorageBackendError::RetentionFailed {
                reason: "injected".to_owned(),
                detail: String::new(),
                instance: "recording".to_owned(),
            });
        }
        self.passes
            .lock()
            .expect("no panics under this lock")
            .push((
                request.topic().to_owned(),
                request.partition(),
                request.max_stored_bytes(),
            ));
        Ok(RetentionReport {
            removed_events: 2,
            removed_bytes: 200,
            remaining_events: 8,
            remaining_bytes: 800,
            oldest_surviving_sequence: Some(Sequence::assigned(3)),
        })
    }
}

struct FixedResolver {
    backend: Arc<dyn EventBrokerBackend>,
}

impl BackendResolver for FixedResolver {
    fn resolve(&self, _topic: &Topic) -> Arc<dyn EventBrokerBackend> {
        Arc::clone(&self.backend)
    }
}

fn config(topics: &serde_json::Value) -> EventBrokerConfig {
    serde_json::from_value(json!({
        "mode": "standalone",
        "default_storage_backend": "gts.cf.core.events.backend.v1~cf.core.backend.sqlite.v1~",
        "topics": topics.clone(),
    }))
    .expect("test configuration deserializes")
}

fn worker(
    topics: Vec<Topic>,
    cfg: EventBrokerConfig,
    backend: Arc<RecordingBackend>,
) -> RetentionWorker {
    RetentionWorker::new(
        Arc::new(StubSpecs { topics }),
        Arc::new(FixedResolver {
            backend: backend as Arc<dyn EventBrokerBackend>,
        }),
        cfg,
        Duration::from_mins(1),
    )
}

#[tokio::test]
async fn one_sweep_drives_one_pass_per_configured_partition() {
    let backend = Arc::new(RecordingBackend::default());
    let worker = worker(
        vec![topic(TOPIC, 8)],
        config(&json!({
            TOPIC_TYPE: {
                "partitions": 3,
                "retention": { "duration": "30d", "size_bytes": 128_000_000 },
            },
        })),
        Arc::clone(&backend),
    );

    let report = worker.run_once().await;

    assert_eq!(
        report,
        super::SweepReport {
            passes: 3,
            removed_events: 6,
            removed_bytes: 600,
            failures: 0,
        }
    );
    assert_eq!(
        *backend.passes.lock().expect("no panics under this lock"),
        vec![
            (TOPIC.to_owned(), 0, Some(128_000_000)),
            (TOPIC.to_owned(), 1, Some(128_000_000)),
            (TOPIC.to_owned(), 2, Some(128_000_000)),
        ],
        "the partition count and the byte bound both come from configuration, \
         not from the topic's registry entity - which says 8 partitions here"
    );
}

#[tokio::test]
async fn a_topic_with_no_size_bound_is_driven_without_one() {
    let backend = Arc::new(RecordingBackend::default());
    let worker = worker(
        vec![topic(TOPIC, 1)],
        config(&json!({
            TOPIC_TYPE: {
                "partitions": 1,
                "retention": { "duration": "1h" },
            },
        })),
        Arc::clone(&backend),
    );

    worker.run_once().await;

    assert_eq!(
        *backend.passes.lock().expect("no panics under this lock"),
        vec![(TOPIC.to_owned(), 0, None)]
    );
}

#[tokio::test]
async fn several_driven_sweeps_run_exactly_as_many_passes_as_were_asked_for() {
    let backend = Arc::new(RecordingBackend::default());
    let worker = worker(
        vec![topic(TOPIC, 1)],
        config(&json!({ TOPIC_TYPE: { "partitions": 2 } })),
        Arc::clone(&backend),
    );

    // Three sweeps, forced. Nothing is spawned and nothing sleeps, so a count
    // of six is arithmetic rather than a timing observation.
    for _ in 0..3 {
        worker.run_once().await;
    }

    assert_eq!(
        backend
            .passes
            .lock()
            .expect("no panics under this lock")
            .len(),
        6,
        "three sweeps over two partitions"
    );
}

/// A topic whose type no entry names is not left unbounded: configuration
/// always supplies a count, so the sweep reaches it under the built-in tier.
/// `OTHER_TOPIC` is of a different type here, so nothing but the built-in
/// entry can reach it.
#[tokio::test]
async fn a_topic_no_entry_names_is_swept_under_the_built_in_count() {
    let backend = Arc::new(RecordingBackend::default());
    let worker = worker(
        vec![topic(TOPIC, 1), topic(OTHER_TOPIC, 1)],
        config(&json!({ TOPIC_TYPE: { "partitions": 1 } })),
        Arc::clone(&backend),
    );

    let report = worker.run_once().await;

    assert_eq!(
        report,
        super::SweepReport {
            passes: 9,
            removed_events: 18,
            removed_bytes: 1800,
            failures: 0,
        },
        "one partition for the configured topic, and the built-in eight for the other"
    );
    let mut expected = vec![(TOPIC.to_owned(), 0, None)];
    expected.extend((0..8_u32).map(|partition| (OTHER_TOPIC.to_owned(), partition, None)));
    assert_eq!(
        *backend.passes.lock().expect("no panics under this lock"),
        expected,
        "both topics are bounded, the second by the count nobody had to write"
    );
}

#[tokio::test]
async fn a_failing_pass_is_counted_and_the_sweep_carries_on() {
    let backend = Arc::new(RecordingBackend {
        fail_every_pass: true,
        ..RecordingBackend::default()
    });
    let worker = worker(
        vec![topic(TOPIC, 1)],
        config(&json!({ TOPIC_TYPE: { "partitions": 4 } })),
        Arc::clone(&backend),
    );

    let report = worker.run_once().await;

    assert_eq!(
        report,
        super::SweepReport {
            passes: 0,
            removed_events: 0,
            removed_bytes: 0,
            failures: 4,
        },
        "one unhappy partition must not leave the other three unbounded until \
         the next tick"
    );
    assert_eq!(backend.failures.load(Ordering::Relaxed), 4);
}

/// A deployment that configures no topics at all still bounds them. The
/// fixture's own four partitions are ignored, which is the other half of the
/// point: the count comes from configuration, and configuration is never
/// absent.
#[tokio::test]
async fn an_empty_topics_map_sweeps_every_topic_under_the_built_in_count() {
    let backend = Arc::new(RecordingBackend::default());
    let worker = worker(
        vec![topic(TOPIC, 4)],
        config(&json!({})),
        Arc::clone(&backend),
    );

    let report = worker.run_once().await;

    assert_eq!(
        report,
        super::SweepReport {
            passes: 8,
            removed_events: 16,
            removed_bytes: 1600,
            failures: 0,
        }
    );
    assert_eq!(
        *backend.passes.lock().expect("no panics under this lock"),
        (0..8_u32)
            .map(|partition| (TOPIC.to_owned(), partition, None))
            .collect::<Vec<_>>(),
    );
}
