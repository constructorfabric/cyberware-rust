mod builder;
mod commit;
mod dispatcher;
mod offset_manager;
mod progress;
mod runtime;
mod type_cache;
mod types;

#[cfg(test)]
mod batch_tests;
#[cfg(test)]
mod builder_tests;
#[cfg(test)]
mod commit_tests;
#[cfg(test)]
#[cfg(feature = "test-util")]
mod dispatcher_test_util;
#[cfg(test)]
mod dispatcher_tests;
#[cfg(test)]
#[cfg(feature = "db")]
mod offset_manager_tests;
#[cfg(test)]
mod progress_tests;
#[cfg(test)]
#[cfg(feature = "test-util")]
mod type_cache_tests;
#[cfg(test)]
mod types_tests;

#[cfg(feature = "db")]
pub use commit::TxCommitHandle;

#[cfg(feature = "db")]
pub use builder::WithTx;
pub use builder::{
    BrokerOnly, ConsumerBatchReady, ConsumerBuilder, ConsumerOffsetManager, ConsumerReady,
    ConsumerRoute, ConsumerRouteBuilder, ConsumerRouteHandlerKind, ConsumerRoutedReady,
    NoDefaultHandler, RouteHasTopic, RouteMissingTopic,
};
#[cfg(feature = "db")]
pub use builder::{TxConsumerRouteBuilder, TxConsumerRoutedReady};

pub use offset_manager::{CommitOffset, Fallback, InMemoryOffsetManager, OffsetStore};
#[cfg(feature = "db")]
pub use offset_manager::{
    CommitOffsetInTx, LOCAL_DB_OFFSET_STORE_MIGRATION_SQL, LocalDbOffsetManager,
};
pub use runtime::{Consumer, ConsumerHandle};

pub use crate::api::{
    BarrierMode, ControlCode, FrameStream, PartitionPosition, Position, SeekPosition,
    SubscriptionAssignment, TenantTraversalDepth, WireEvent, WireFrame,
};
pub use crate::error::OffsetManagerError;
pub use types::{
    BatchHandlerOutcome, ConnectionDropReason, ConsumerBatching, ConsumerBuffering,
    ConsumerCommitMode, ConsumerGroupRef, ConsumerHandler, ConsumerListenerSettings,
    ConsumerProfile, ConsumerRetry, ConsumerRuntimeEvent, ConsumerRuntimeListener,
    ConsumerSettings, ConsumerSettingsOverrides, ConsumerSlowDetection, EventBatch, EventTypeRef,
    FilterEngineRef, HandlerOutcome, PartitionBufferState, PartitionBufferStateSnapshot,
    PartitionProgress, RawEvent, SingleEventHandler, SingleEventHandlerAdapter,
    SlowConsumerTrigger, SubscriptionFilterRef, SubscriptionInterest, TopicRef,
};
#[cfg(feature = "db")]
pub use types::{TxConsumerHandler, TxSingleEventHandler, TxSingleEventHandlerAdapter};
