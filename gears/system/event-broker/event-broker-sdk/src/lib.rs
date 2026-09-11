//! Event Broker SDK
//!
//! High-level typed event publishing and consumption for the `event-broker` gear.
//!
//! See [`EventBrokerApi`] for the entry point; obtain it from `ClientHub`:
//! ```ignore
//! let broker = hub.get::<dyn EventBrokerApi>()?;
//! ```

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

pub mod api;
pub mod consumer;
pub mod dlq;
pub mod error;
pub mod gts;
pub mod ids;
pub mod models;
pub mod producer;
pub mod sdk;
pub mod sequence;
pub mod typed_event;
pub mod validate;

#[cfg(test)]
mod api_tests;

#[cfg(test)]
mod gts_tests;

#[cfg(test)]
mod sequence_tests;

#[cfg(test)]
mod validate_tests;

#[cfg(feature = "test-util")]
pub mod mock;

#[cfg(feature = "rest-client")]
pub mod rest;

pub use api::{
    AssignedPartition, BarrierMode, EventBrokerApi, EventBrokerBackend, EventBrokerBackendProvider,
    IngestOutcome, JoinRequest, PartitionCursor, Position, ProducerCursors, ProducerMode,
    RetentionReport, RetentionRequest, RetentionRequestBuilder, SeekResult, StorageBackendConfig,
    SubscriptionAssignment, TenantTraversalDepth, TopicCursors,
};
pub use consumer::{
    BatchHandlerOutcome, CommitOffset, ConnectionDropReason, Consumer, ConsumerBatching,
    ConsumerBuffering, ConsumerBuilder, ConsumerCommitMode, ConsumerGroupRef, ConsumerHandler,
    ConsumerListenerSettings, ConsumerProfile, ConsumerRetry, ConsumerRuntimeEvent,
    ConsumerRuntimeListener, ConsumerSettings, ConsumerSettingsOverrides, ConsumerSlowDetection,
    ControlCode, EventBatch, EventTypeRef, Fallback, FilterEngineRef, FrameStream, HandlerOutcome,
    InMemoryOffsetManager, OffsetStore, PartitionBufferState, PartitionBufferStateSnapshot,
    PartitionPosition, PartitionProgress, RawEvent, SeekPosition, SingleEventHandler,
    SlowConsumerTrigger, SubscriptionFilterRef, SubscriptionInterest, TopicRef, WireEvent,
    WireFrame,
};
#[cfg(feature = "db")]
pub use consumer::{
    CommitOffsetInTx, LocalDbOffsetManager, TxCommitHandle, TxConsumerHandler,
    TxSingleEventHandler, WithTx,
};

pub use error::{
    ConsumerError, EventBrokerError, OffsetManagerError, OutOfRange, PositionViolation,
    StorageBackendError,
};
pub use gts::{
    CONSUMER_GROUP_RESOURCE_TYPE, EVENT_TYPE_RESOURCE_TYPE, PRODUCER_RESOURCE_TYPE,
    REQUEST_RESOURCE_TYPE, SUBSCRIPTION_RESOURCE_TYPE, TopicV1,
};
pub use ids::{ConsumerGroupId, EventTypeId, ProducerId, SubscriptionId, TopicId};
pub use models::{
    ConsumerGroup, ConsumerGroupKind, ConsumerGroupQuery, CreateConsumerGroupRequest, Event, Page,
    PartitionAssignment, PartitionLeader, PartitionRange, ResetScope, Subscription, Topic,
    TopicSegment,
};
#[cfg(feature = "db")]
pub use producer::{
    DbDeduplication, DbProducer, DbProducerBuilder, ManagedDeduplication,
    MissingProducerRegistration, UnknownProducerRegistration, producer_registration_migrations,
};
pub use producer::{
    DirectDeduplication, Producer, ProducerBuilder, ProducerIdentity, ValidationTiming,
};
#[cfg(feature = "outbox")]
pub use producer::{ProducerOutbox, ProducerOutboxHandle, ProducerOutboxQueue};
pub use sdk::EventBrokerSdk;
pub use sequence::Sequence;
pub use typed_event::{EnvelopedEvent, TypedEvent};
