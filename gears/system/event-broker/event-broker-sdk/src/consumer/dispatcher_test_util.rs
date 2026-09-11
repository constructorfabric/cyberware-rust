//! Test helpers used exclusively by `#[cfg(feature = "test-util")]` dispatcher integration tests.

use crate::sequence::Sequence;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{
    BatchHandlerOutcome, CommitOffset, ConsumerHandler, ConsumerRuntimeEvent,
    ConsumerRuntimeListener, EventBatch, OffsetManagerError, OffsetStore, Position,
};
use crate::error::ConsumerError;
use crate::ids::{ConsumerGroupId, TopicId};

pub(super) type SharedAttempts = Arc<Mutex<Vec<u16>>>;
pub(super) type SharedTimeline = Arc<Mutex<Vec<&'static str>>>;
pub(super) type CommitRecord = (ConsumerGroupId, TopicId, u32, i64);
pub(super) type SharedCommits = Arc<Mutex<Vec<CommitRecord>>>;
pub(super) type SharedScopes = Arc<Mutex<Vec<(String, u32, usize)>>>;
pub(super) type SharedViolations = Arc<Mutex<Vec<String>>>;
pub(super) type SharedRuntimeEvents = Arc<Mutex<Vec<ConsumerRuntimeEvent>>>;

pub(super) fn partition_key_for_partition(target: u32, partitions: u32) -> String {
    assert_eq!(partitions, 2, "only the two-partition fixture is supported");
    match target {
        0 => "partition-key-0-1",
        1 => "partition-key-1-0",
        _ => panic!("two-partition fixture cannot target partition {target}"),
    }
    .to_owned()
}

type SharedPartitionCalls = Arc<Mutex<Vec<(String, u32, i64)>>>;

pub(super) struct SleepingBatchHandler {
    pub(super) calls: SharedPartitionCalls,
    pub(super) delay: Duration,
}

#[async_trait::async_trait]
impl ConsumerHandler for SleepingBatchHandler {
    async fn handle_batch(
        &self,
        batch: &EventBatch<'_>,
        _attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError> {
        tokio::time::sleep(self.delay).await;
        let chunk = batch.next_chunk(batch.len());
        self.calls.lock().unwrap().extend(
            chunk
                .iter()
                .map(|event| (event.topic.clone(), event.partition, event.offset.as_i64())),
        );
        Ok(chunk
            .last()
            .map(|event| BatchHandlerOutcome::AdvanceThrough {
                offset: event.offset,
            })
            .unwrap_or(BatchHandlerOutcome::Success))
    }
}

pub(super) struct FailingThenCommitBatchHandler {
    pub(super) failures_remaining: Arc<Mutex<usize>>,
    pub(super) calls: SharedAttempts,
}

#[async_trait::async_trait]
impl ConsumerHandler for FailingThenCommitBatchHandler {
    async fn handle_batch(
        &self,
        _batch: &EventBatch<'_>,
        attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError> {
        {
            let mut guard = self.failures_remaining.lock().unwrap();
            if *guard > 0 {
                *guard -= 1;
                return Err(ConsumerError::Internal(
                    "intentional representative handler failure".to_owned(),
                ));
            }
        }

        self.calls.lock().unwrap().push(attempts);
        Ok(BatchHandlerOutcome::Success)
    }
}

pub(super) struct SequencedOffsetManager {
    pub(super) timeline: SharedTimeline,
}

#[async_trait::async_trait]
impl OffsetStore for SequencedOffsetManager {
    async fn load_position(
        &self,
        _group: &ConsumerGroupId,
        _topic: &TopicId,
        _partition: u32,
    ) -> Result<Position, OffsetManagerError> {
        self.timeline.lock().unwrap().push("load");
        Ok(Position::Earliest)
    }
}

#[async_trait::async_trait]
impl CommitOffset for SequencedOffsetManager {
    async fn commit(
        &self,
        _group: &ConsumerGroupId,
        _topic: &TopicId,
        _partition: u32,
        _offset: Sequence,
    ) -> Result<(), OffsetManagerError> {
        Ok(())
    }
}

#[derive(Clone, Default)]
pub(super) struct RecordingCommitOffsetManager {
    pub(super) commits: SharedCommits,
}

#[async_trait::async_trait]
impl OffsetStore for RecordingCommitOffsetManager {
    async fn load_position(
        &self,
        _group: &ConsumerGroupId,
        _topic: &TopicId,
        _partition: u32,
    ) -> Result<Position, OffsetManagerError> {
        Ok(Position::Earliest)
    }
}

#[async_trait::async_trait]
impl CommitOffset for RecordingCommitOffsetManager {
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

pub(super) struct SequencedBatchHandler {
    pub(super) timeline: SharedTimeline,
}

#[async_trait::async_trait]
impl ConsumerHandler for SequencedBatchHandler {
    async fn handle_batch(
        &self,
        _batch: &EventBatch<'_>,
        _attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError> {
        self.timeline.lock().unwrap().push("handle");
        Ok(BatchHandlerOutcome::Success)
    }
}

#[derive(Default)]
pub(super) struct BatchScopeRecorder {
    pub(super) scopes: SharedScopes,
    pub(super) violations: SharedViolations,
}

#[async_trait::async_trait]
impl ConsumerHandler for BatchScopeRecorder {
    async fn handle_batch(
        &self,
        batch: &EventBatch<'_>,
        _attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError> {
        let chunk = batch.next_chunk(batch.len());
        if let Some(first) = chunk.first() {
            if chunk
                .iter()
                .any(|event| event.topic != first.topic || event.partition != first.partition)
            {
                self.violations
                    .lock()
                    .unwrap()
                    .push("batch mixed topic IDs or partitions".to_owned());
            }
            self.scopes
                .lock()
                .unwrap()
                .push((first.topic.clone(), first.partition, chunk.len()));
        }
        Ok(chunk
            .last()
            .map(|event| BatchHandlerOutcome::AdvanceThrough {
                offset: event.offset,
            })
            .unwrap_or(BatchHandlerOutcome::Success))
    }
}

#[derive(Clone, Default)]
pub(super) struct RecordingRuntimeListener {
    pub(super) events: SharedRuntimeEvents,
}

#[async_trait::async_trait]
impl ConsumerRuntimeListener for RecordingRuntimeListener {
    async fn on_consumer_event(&self, event: &ConsumerRuntimeEvent) -> Result<(), ConsumerError> {
        self.events.lock().unwrap().push(event.clone());
        Ok(())
    }
}

pub(super) struct FailingRuntimeListener;

#[async_trait::async_trait]
impl ConsumerRuntimeListener for FailingRuntimeListener {
    async fn on_consumer_event(&self, _event: &ConsumerRuntimeEvent) -> Result<(), ConsumerError> {
        Err(ConsumerError::Internal(
            "intentional listener failure".to_owned(),
        ))
    }
}

pub(super) struct SlowRuntimeListener {
    pub(super) delay: Duration,
}

#[async_trait::async_trait]
impl ConsumerRuntimeListener for SlowRuntimeListener {
    async fn on_consumer_event(&self, _event: &ConsumerRuntimeEvent) -> Result<(), ConsumerError> {
        tokio::time::sleep(self.delay).await;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum RuntimeEventKind {
    SubscriptionJoining,
    SubscriptionStarted,
    SubscriptionRejoining,
    SubscriptionTerminated,
    SubscriptionConnectionDropped,
    AssignmentChanged,
    ProgressAdvanced,
    PartitionBufferStateChanged,
    HandlerBatchStarted,
    HandlerBatchCompleted,
    HandlerFailed,
    OffsetLoaded,
    OffsetCommitted,
    RetryScheduled,
}

pub(super) fn runtime_event_kind(event: &ConsumerRuntimeEvent) -> RuntimeEventKind {
    match event {
        ConsumerRuntimeEvent::SubscriptionJoining { .. } => RuntimeEventKind::SubscriptionJoining,
        ConsumerRuntimeEvent::SubscriptionStarted { .. } => RuntimeEventKind::SubscriptionStarted,
        ConsumerRuntimeEvent::SubscriptionRejoining { .. } => {
            RuntimeEventKind::SubscriptionRejoining
        }
        ConsumerRuntimeEvent::SubscriptionTerminated { .. } => {
            RuntimeEventKind::SubscriptionTerminated
        }
        ConsumerRuntimeEvent::SubscriptionConnectionDropped { .. } => {
            RuntimeEventKind::SubscriptionConnectionDropped
        }
        ConsumerRuntimeEvent::AssignmentChanged { .. } => RuntimeEventKind::AssignmentChanged,
        ConsumerRuntimeEvent::ProgressAdvanced { .. } => RuntimeEventKind::ProgressAdvanced,
        ConsumerRuntimeEvent::PartitionBufferStateChanged { .. } => {
            RuntimeEventKind::PartitionBufferStateChanged
        }
        ConsumerRuntimeEvent::HandlerBatchStarted { .. } => RuntimeEventKind::HandlerBatchStarted,
        ConsumerRuntimeEvent::HandlerBatchCompleted { .. } => {
            RuntimeEventKind::HandlerBatchCompleted
        }
        ConsumerRuntimeEvent::HandlerFailed { .. } => RuntimeEventKind::HandlerFailed,
        ConsumerRuntimeEvent::OffsetLoaded { .. } => RuntimeEventKind::OffsetLoaded,
        ConsumerRuntimeEvent::OffsetCommitted { .. } => RuntimeEventKind::OffsetCommitted,
        ConsumerRuntimeEvent::RetryScheduled { .. } => RuntimeEventKind::RetryScheduled,
    }
}
