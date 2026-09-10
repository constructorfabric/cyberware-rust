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
pub mod typed_event;

#[cfg(test)]
mod api_tests;

#[cfg(test)]
mod gts_tests;

#[cfg(feature = "test-util")]
pub mod mock;

pub use api::{
    AssignedPartition, BarrierMode, EventBrokerApi, EventBrokerBackend, IngestOutcome, JoinRequest,
    PartitionCursor, ProducerCursors, ProducerMode, ResolvedPosition, SeekResult,
    StorageBackendConfig, SubscriptionAssignment, TenantTraversalDepth, TopicCursors,
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

pub use error::{ConsumerError, EventBrokerError, OffsetManagerError, StorageBackendError};
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
pub use typed_event::{EnvelopedEvent, TypedEvent};
