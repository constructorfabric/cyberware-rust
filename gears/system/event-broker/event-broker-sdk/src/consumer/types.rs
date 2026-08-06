use std::sync::Arc;
use std::time::Duration;
use toolkit_gts::gts_id;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[cfg(feature = "db")]
use super::commit::TxCommitHandle;
#[cfg(feature = "db")]
use super::offset_manager::CommitOffsetInTx;
use crate::api::{AssignedPartition, Position};
use crate::error::{ConsumerError, EventBrokerError};
use crate::ids::{ConsumerGroupId, EventTypeId, SubscriptionId, TopicId};
use crate::sequence::Sequence;

/// Raw event delivered to v1 handlers. `data` is untyped JSON;
/// typed dispatch is deferred to v2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEvent {
    pub id: Uuid,
    pub type_id: String,
    pub topic: String,
    pub tenant_id: Uuid,
    pub subject: String,
    pub subject_type: String,
    pub partition: u32,
    pub sequence: Sequence,
    pub offset: Sequence,
    pub occurred_at: DateTime<Utc>,
    pub sequence_time: DateTime<Utc>,
    pub trace_parent: Option<String>,
    pub data: serde_json::Value,
}

/// Handler outcome without DLQ - `Reject` is structurally absent.
#[derive(Debug, Clone)]
pub enum HandlerOutcome {
    Success,
    Retry { reason: String },
}

/// Batch handler outcome. Partial progress is always the contiguous delivered
/// prefix through the given offset.
#[derive(Debug, Clone)]
pub enum BatchHandlerOutcome {
    Success,
    AdvanceThrough { offset: Sequence },
    Retry { reason: String },
}

/// Read-only view over one topic-partition batch. Stateless: each delivery constructs
/// a fresh view, and progress is tracked externally via the handler outcome offset and
/// the committed partition frontier (not by any per-batch cursor).
pub struct EventBatch<'a> {
    events: &'a [RawEvent],
}

impl<'a> EventBatch<'a> {
    pub fn new(events: &'a [RawEvent]) -> Self {
        Self { events }
    }

    pub fn next_event(&self) -> Option<&'a RawEvent> {
        self.events.first()
    }

    pub fn next_chunk(&self, n: usize) -> &'a [RawEvent] {
        &self.events[..n.min(self.events.len())]
    }

    pub fn iter(&self) -> impl Iterator<Item = &'a RawEvent> {
        self.events.iter()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Tracks the committed frontier for one topic partition.
#[derive(Debug, Clone)]
pub(crate) struct PartitionFrontier {
    committed: Sequence,
}

impl PartitionFrontier {
    pub(crate) fn new(committed: Sequence) -> Self {
        Self { committed }
    }

    pub(crate) fn committed(&self) -> Sequence {
        self.committed
    }
}

/// Topic reference accepted by the consumer builder before registry resolution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TopicRef {
    Id(TopicId),
    Gts(String),
}

impl TopicRef {
    pub fn id(id: TopicId) -> Self {
        Self::Id(id)
    }

    pub fn gts(gts: impl Into<String>) -> Self {
        Self::Gts(gts.into())
    }
}

impl From<TopicId> for TopicRef {
    fn from(value: TopicId) -> Self {
        Self::Id(value)
    }
}

/// Event-type reference accepted by the consumer builder before registry resolution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EventTypeRef {
    Id(EventTypeId),
    Gts(String),
    GtsPattern(String),
}

impl EventTypeRef {
    pub fn id(id: EventTypeId) -> Self {
        Self::Id(id)
    }

    pub fn gts(gts: impl Into<String>) -> Self {
        Self::Gts(gts.into())
    }

    pub fn gts_pattern(pattern: impl Into<String>) -> Self {
        Self::GtsPattern(pattern.into())
    }
}

impl From<EventTypeId> for EventTypeRef {
    fn from(value: EventTypeId) -> Self {
        Self::Id(value)
    }
}

/// Consumer-group reference for the builder.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConsumerGroupRef {
    Id(ConsumerGroupId),
    Gts(String),
    AutoAnonymous { alias: String },
}

impl ConsumerGroupRef {
    pub fn id(id: ConsumerGroupId) -> Self {
        Self::Id(id)
    }

    pub fn existing(id: ConsumerGroupId) -> Self {
        Self::Id(id)
    }

    pub fn gts(gts: impl Into<String>) -> Self {
        Self::Gts(gts.into())
    }

    pub fn auto_anonymous(alias: impl Into<String>) -> Self {
        Self::AutoAnonymous {
            alias: alias.into(),
        }
    }
}

impl From<ConsumerGroupId> for ConsumerGroupRef {
    fn from(value: ConsumerGroupId) -> Self {
        Self::Id(value)
    }
}

/// Broker-side filter engine reference accepted before registry resolution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FilterEngineRef {
    Id(Uuid),
    Gts(String),
}

impl FilterEngineRef {
    pub fn id(id: Uuid) -> Self {
        Self::Id(id)
    }

    pub fn gts(gts: impl Into<String>) -> Self {
        Self::Gts(gts.into())
    }
}

/// Per-interest imperative subscription filter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubscriptionFilterRef {
    pub engine: FilterEngineRef,
    pub expression: String,
}

impl SubscriptionFilterRef {
    pub fn new(engine: FilterEngineRef, expression: impl Into<String>) -> Self {
        Self {
            engine,
            expression: expression.into(),
        }
    }

    pub fn cel(expression: impl Into<String>) -> Self {
        Self::new(
            FilterEngineRef::gts(gts_id!(
                "cf.core.events.filter.v1~cf.core.expression.cel.v1"
            )),
            expression,
        )
    }
}

/// Topic-scoped consumer interest. Event type selectors and filters belong to
/// exactly one topic.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubscriptionInterest {
    pub topic: TopicRef,
    pub event_types: Vec<EventTypeRef>,
    pub filter: Option<SubscriptionFilterRef>,
}

impl SubscriptionInterest {
    pub fn builder() -> SubscriptionInterestBuilder {
        SubscriptionInterestBuilder::default()
    }
}

#[derive(Default)]
pub struct SubscriptionInterestBuilder {
    topic: Option<TopicRef>,
    event_types: Vec<EventTypeRef>,
    filter: Option<SubscriptionFilterRef>,
}

impl SubscriptionInterestBuilder {
    pub fn topic(mut self, topic: impl Into<TopicRef>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    pub fn types<I>(mut self, event_types: I) -> Self
    where
        I: IntoIterator<Item = EventTypeRef>,
    {
        self.event_types.extend(event_types);
        self
    }

    pub fn filter(mut self, filter: SubscriptionFilterRef) -> Self {
        self.filter = Some(filter);
        self
    }

    pub fn build(self) -> Result<SubscriptionInterest, EventBrokerError> {
        let topic = self
            .topic
            .ok_or_else(|| EventBrokerError::InvalidConsumerOptions {
                detail: "subscription interest requires a topic".to_owned(),
                instance: String::new(),
            })?;
        if self.event_types.is_empty() {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: "subscription interest requires at least one event type selector"
                    .to_owned(),
                instance: String::new(),
            });
        }
        Ok(SubscriptionInterest {
            topic,
            event_types: self.event_types,
            filter: self.filter,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerProfile {
    pub buffering: ConsumerBuffering,
    pub batching: ConsumerBatching,
    pub slow_detection: ConsumerSlowDetection,
    pub retry: ConsumerRetry,
    pub listener: ConsumerListenerSettings,
}

impl ConsumerProfile {
    pub fn default_profile() -> Self {
        Self {
            buffering: ConsumerBuffering {
                partition_capacity: 256,
                high_watermark: 205,
                low_watermark: 128,
            },
            batching: ConsumerBatching {
                max_events: 1,
                max_wait: Duration::from_millis(0),
            },
            slow_detection: ConsumerSlowDetection {
                handler_latency: Duration::from_secs(5),
                handler_strikes: 3,
            },
            retry: ConsumerRetry {
                base_delay: Duration::from_secs(1),
                max_delay: Duration::from_secs(60),
            },
            listener: ConsumerListenerSettings {
                timeout: Duration::from_millis(250),
                channel_capacity: 1024,
            },
        }
    }

    pub fn low_latency() -> Self {
        Self {
            buffering: ConsumerBuffering {
                partition_capacity: 128,
                high_watermark: 96,
                low_watermark: 48,
            },
            batching: ConsumerBatching {
                max_events: 1,
                max_wait: Duration::from_millis(0),
            },
            slow_detection: ConsumerSlowDetection {
                handler_latency: Duration::from_secs(1),
                handler_strikes: 2,
            },
            retry: ConsumerRetry {
                base_delay: Duration::from_millis(100),
                max_delay: Duration::from_secs(5),
            },
            listener: ConsumerListenerSettings {
                timeout: Duration::from_millis(100),
                channel_capacity: 2048,
            },
        }
    }

    pub fn high_throughput() -> Self {
        Self {
            buffering: ConsumerBuffering {
                partition_capacity: 1024,
                high_watermark: 819,
                low_watermark: 512,
            },
            batching: ConsumerBatching {
                max_events: 128,
                max_wait: Duration::from_millis(500),
            },
            slow_detection: ConsumerSlowDetection {
                handler_latency: Duration::from_secs(10),
                handler_strikes: 5,
            },
            retry: ConsumerRetry {
                base_delay: Duration::from_secs(1),
                max_delay: Duration::from_secs(120),
            },
            listener: ConsumerListenerSettings {
                timeout: Duration::from_millis(500),
                channel_capacity: 4096,
            },
        }
    }

    pub fn replay() -> Self {
        Self {
            buffering: ConsumerBuffering {
                partition_capacity: 4096,
                high_watermark: 3584,
                low_watermark: 2048,
            },
            batching: ConsumerBatching {
                max_events: 512,
                max_wait: Duration::from_secs(1),
            },
            slow_detection: ConsumerSlowDetection {
                handler_latency: Duration::from_secs(60),
                handler_strikes: 10,
            },
            retry: ConsumerRetry {
                base_delay: Duration::from_secs(2),
                max_delay: Duration::from_secs(300),
            },
            listener: ConsumerListenerSettings {
                timeout: Duration::from_millis(500),
                channel_capacity: 1024,
            },
        }
    }

    pub fn relaxed() -> Self {
        Self {
            buffering: ConsumerBuffering {
                partition_capacity: 128,
                high_watermark: 102,
                low_watermark: 64,
            },
            batching: ConsumerBatching {
                max_events: 16,
                max_wait: Duration::from_secs(2),
            },
            slow_detection: ConsumerSlowDetection {
                handler_latency: Duration::from_secs(30),
                handler_strikes: 5,
            },
            retry: ConsumerRetry {
                base_delay: Duration::from_secs(5),
                max_delay: Duration::from_secs(300),
            },
            listener: ConsumerListenerSettings {
                timeout: Duration::from_millis(250),
                channel_capacity: 256,
            },
        }
    }
}

impl Default for ConsumerProfile {
    fn default() -> Self {
        Self::default_profile()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerSettings {
    pub buffering: ConsumerBuffering,
    pub batching: ConsumerBatching,
    pub slow_detection: ConsumerSlowDetection,
    pub retry: ConsumerRetry,
    pub listener: ConsumerListenerSettings,
}

impl ConsumerSettings {
    pub fn from_profile(profile: ConsumerProfile) -> Self {
        Self {
            buffering: profile.buffering,
            batching: profile.batching,
            slow_detection: profile.slow_detection,
            retry: profile.retry,
            listener: profile.listener,
        }
    }

    pub fn resolve(profile: ConsumerProfile, overrides: ConsumerSettingsOverrides) -> Self {
        Self {
            buffering: overrides.buffering.unwrap_or(profile.buffering),
            batching: overrides.batching.unwrap_or(profile.batching),
            slow_detection: overrides.slow_detection.unwrap_or(profile.slow_detection),
            retry: overrides.retry.unwrap_or(profile.retry),
            listener: overrides.listener.unwrap_or(profile.listener),
        }
    }

    pub fn validate(&self) -> Result<(), EventBrokerError> {
        if self.buffering.partition_capacity == 0 {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: "partition buffer capacity must be greater than zero".to_owned(),
                instance: String::new(),
            });
        }
        if self.buffering.high_watermark > self.buffering.partition_capacity {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: "buffer high watermark must not exceed partition capacity".to_owned(),
                instance: String::new(),
            });
        }
        if self.buffering.low_watermark > self.buffering.high_watermark {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: "buffer low watermark must not exceed high watermark".to_owned(),
                instance: String::new(),
            });
        }
        if self.batching.max_events == 0 {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: "batching max_events must be greater than zero".to_owned(),
                instance: String::new(),
            });
        }
        if self.slow_detection.handler_strikes == 0 {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: "slow detection handler_strikes must be greater than zero".to_owned(),
                instance: String::new(),
            });
        }
        if self.retry.max_delay < self.retry.base_delay {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: "retry max_delay must be greater than or equal to base_delay".to_owned(),
                instance: String::new(),
            });
        }
        if self.listener.channel_capacity == 0 {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: "listener channel capacity must be greater than zero".to_owned(),
                instance: String::new(),
            });
        }
        if self.listener.timeout.is_zero() {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: "listener timeout must be greater than zero".to_owned(),
                instance: String::new(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConsumerSettingsOverrides {
    pub buffering: Option<ConsumerBuffering>,
    pub batching: Option<ConsumerBatching>,
    pub slow_detection: Option<ConsumerSlowDetection>,
    pub retry: Option<ConsumerRetry>,
    pub listener: Option<ConsumerListenerSettings>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumerBuffering {
    pub partition_capacity: usize,
    pub high_watermark: usize,
    pub low_watermark: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumerBatching {
    pub max_events: usize,
    pub max_wait: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumerSlowDetection {
    pub handler_latency: Duration,
    pub handler_strikes: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumerRetry {
    pub base_delay: Duration,
    pub max_delay: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerCommitMode {
    Auto { interval: Duration },
    Manual,
}

impl ConsumerCommitMode {
    pub fn auto(interval: Duration) -> Self {
        Self::Auto { interval }
    }

    pub fn manual() -> Self {
        Self::Manual
    }
}

impl Default for ConsumerCommitMode {
    fn default() -> Self {
        Self::Auto {
            interval: Duration::from_secs(20),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumerListenerSettings {
    pub timeout: Duration,
    pub channel_capacity: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionBufferState {
    Connected,
    SlowDetected,
    ConnectionDropped,
    Draining,
    Rejoining,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlowConsumerTrigger {
    BufferHighWatermark,
    HandlerLatencyStrikes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionBufferStateSnapshot {
    pub group_id: ConsumerGroupId,
    pub subscription_id: SubscriptionId,
    pub topic_id: TopicId,
    pub topic: String,
    pub partition: u32,
    pub state: PartitionBufferState,
    pub trigger: Option<SlowConsumerTrigger>,
    pub buffered_count: usize,
    pub capacity: usize,
    pub latest_observed_offset: Option<Sequence>,
    pub last_delivered_offset: Option<Sequence>,
    pub consecutive_slow_handlers: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionDropReason {
    SlowConsumer {
        topic_id: TopicId,
        topic: String,
        partition: u32,
        trigger: SlowConsumerTrigger,
    },
    HeartbeatThreshold,
    StreamTerminated,
    Transport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionProgress {
    pub topic_id: TopicId,
    pub topic: String,
    pub partition: u32,
    pub offset: Sequence,
}

#[derive(Debug, Clone)]
pub enum ConsumerRuntimeEvent {
    SubscriptionJoining {
        group_id: ConsumerGroupId,
    },
    SubscriptionStarted {
        group_id: ConsumerGroupId,
        subscription_id: SubscriptionId,
        assigned: Vec<AssignedPartition>,
    },
    SubscriptionRejoining {
        group_id: ConsumerGroupId,
        previous_subscription_id: SubscriptionId,
    },
    SubscriptionTerminated {
        subscription_id: SubscriptionId,
        reason: ConnectionDropReason,
    },
    SubscriptionConnectionDropped {
        subscription_id: SubscriptionId,
        reason: ConnectionDropReason,
        affected: Vec<AssignedPartition>,
    },
    AssignmentChanged {
        subscription_id: SubscriptionId,
        assigned: Vec<AssignedPartition>,
    },
    ProgressAdvanced {
        subscription_id: SubscriptionId,
        progress: Vec<PartitionProgress>,
    },
    PartitionBufferStateChanged {
        state: PartitionBufferStateSnapshot,
    },
    HandlerBatchStarted {
        topic_id: TopicId,
        topic: String,
        partition: u32,
        len: usize,
    },
    HandlerBatchCompleted {
        topic_id: TopicId,
        topic: String,
        partition: u32,
        outcome: BatchHandlerOutcome,
    },
    HandlerFailed {
        topic_id: TopicId,
        topic: String,
        partition: u32,
        error: String,
    },
    OffsetLoaded {
        topic_id: TopicId,
        topic: String,
        partition: u32,
        position: Position,
    },
    OffsetCommitted {
        topic_id: TopicId,
        topic: String,
        partition: u32,
        offset: Sequence,
    },
    RetryScheduled {
        topic_id: TopicId,
        topic: String,
        partition: u32,
        attempt: u16,
        delay: Duration,
    },
}

#[async_trait::async_trait]
pub trait ConsumerRuntimeListener: Send + Sync {
    async fn on_consumer_event(&self, event: &ConsumerRuntimeEvent) -> Result<(), ConsumerError>;
}

/// Single-event non-transactional handler. Offset progress is declared by the
/// returned outcome and the SDK adapter maps success to the delivered event offset.
#[async_trait::async_trait]
pub trait SingleEventHandler: Send + Sync {
    async fn handle(&self, event: RawEvent, attempts: u16)
    -> Result<HandlerOutcome, ConsumerError>;
}

/// Native non-transactional batch handler. Implementations receive one
/// topic-partition batch and declare progress through [`BatchHandlerOutcome`].
#[async_trait::async_trait]
pub trait ConsumerHandler: Send + Sync {
    async fn handle_batch(
        &self,
        batch: &EventBatch<'_>,
        attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError>;
}

#[cfg(feature = "db")]
#[async_trait::async_trait]
pub trait TxSingleEventHandler<OM: CommitOffsetInTx>: Send + Sync {
    async fn handle(
        &self,
        event: RawEvent,
        attempts: u16,
        commit: TxCommitHandle<OM>,
    ) -> Result<HandlerOutcome, ConsumerError>;
}

#[cfg(feature = "db")]
#[async_trait::async_trait]
pub trait TxConsumerHandler<OM: CommitOffsetInTx>: Send + Sync {
    async fn handle_batch(
        &self,
        batch: &EventBatch<'_>,
        attempts: u16,
        commit: TxCommitHandle<OM>,
    ) -> Result<HandlerOutcome, ConsumerError>;
}

/// Adapts a single-event handler to the batch-first runtime.
pub struct SingleEventHandlerAdapter<H> {
    inner: Arc<H>,
}

impl<H> SingleEventHandlerAdapter<H> {
    pub fn new(inner: Arc<H>) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &Arc<H> {
        &self.inner
    }
}

#[async_trait::async_trait]
impl<H> ConsumerHandler for SingleEventHandlerAdapter<H>
where
    H: SingleEventHandler + 'static,
{
    async fn handle_batch(
        &self,
        batch: &EventBatch<'_>,
        attempts: u16,
    ) -> Result<BatchHandlerOutcome, ConsumerError> {
        let Some(event) = batch.next_event().cloned() else {
            return Ok(BatchHandlerOutcome::Success);
        };
        let offset = event.offset;

        match self.inner.handle(event, attempts).await? {
            HandlerOutcome::Success => Ok(BatchHandlerOutcome::AdvanceThrough { offset }),
            HandlerOutcome::Retry { reason } => Ok(BatchHandlerOutcome::Retry { reason }),
        }
    }
}

#[cfg(feature = "db")]
pub struct TxSingleEventHandlerAdapter<H, OM: CommitOffsetInTx> {
    inner: Arc<H>,
    _offset_manager: std::marker::PhantomData<OM>,
}

#[cfg(feature = "db")]
impl<H, OM: CommitOffsetInTx> TxSingleEventHandlerAdapter<H, OM> {
    pub fn new(inner: Arc<H>) -> Self {
        Self {
            inner,
            _offset_manager: std::marker::PhantomData,
        }
    }
}

#[cfg(feature = "db")]
#[async_trait::async_trait]
impl<H, OM> TxConsumerHandler<OM> for TxSingleEventHandlerAdapter<H, OM>
where
    H: TxSingleEventHandler<OM> + 'static,
    OM: CommitOffsetInTx + 'static,
{
    async fn handle_batch(
        &self,
        batch: &EventBatch<'_>,
        attempts: u16,
        commit: TxCommitHandle<OM>,
    ) -> Result<HandlerOutcome, ConsumerError> {
        let Some(event) = batch.next_event().cloned() else {
            return Ok(HandlerOutcome::Success);
        };

        self.inner.handle(event, attempts, commit).await
    }
}
