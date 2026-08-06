use std::num::NonZeroU32;
use std::time::Duration;

use async_trait::async_trait;
use gts::GtsInstanceId;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::error::{EventBrokerError, StorageBackendError};
use crate::ids::{ConsumerGroupId, ProducerId, SubscriptionId};
use crate::models::Event;
use crate::models::{
    ConsumerGroup, ConsumerGroupQuery, CreateConsumerGroupRequest, EventType, Page,
    PartitionLeader, PartitionRange, ResetScope, Subscription, Topic, TopicSegment,
};
use crate::sequence::Sequence;

// --- Supporting types ---------------------------------------------------------

/// Producer deduplication mode declared at broker registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProducerMode {
    /// No producer id and no broker-side idempotency metadata.
    Stateless,
    /// Idempotency by `producer_id + sequence`.
    Monotonic,
    /// Idempotency by `producer_id + previous + sequence`.
    #[default]
    Chained,
}

/// Result of a single event publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestOutcome {
    /// The event was admitted before persistence confirmation.
    Accepted,
    /// The event was durably persisted.
    Persisted,
    /// The event matched an idempotent duplicate.
    Duplicate,
}

/// Broker `last_sequence` for one partition of a topic.
#[derive(Debug, Clone)]
pub struct PartitionCursor {
    pub partition: u32,
    pub last_sequence: i64,
}

/// Broker cursors for one topic - the `last_sequence` of each of its partitions.
#[derive(Debug, Clone)]
pub struct TopicCursors {
    pub topic: String,
    pub partitions: Vec<PartitionCursor>,
}

/// Producer cursor snapshot: the broker's known `last_sequence` per
/// `(topic, partition)`, grouped by topic, plus the echoed producer identity.
/// Object-shaped (not a bare array) so producer-level fields can be carried and
/// the response stays extensible.
#[derive(Debug, Clone)]
pub struct ProducerCursors {
    pub producer_id: ProducerId,
    pub client_agent: String,
    pub topics: Vec<TopicCursors>,
}

impl ProducerCursors {
    /// True when the producer has no recorded cursor on any topic/partition.
    pub fn is_empty(&self) -> bool {
        self.topics.iter().all(|topic| topic.partitions.is_empty())
    }

    /// The broker's known `last_sequence` for a `(topic, partition)`, if any.
    pub fn last_sequence(&self, topic: &str, partition: u32) -> Option<i64> {
        self.topics
            .iter()
            .find(|t| t.topic == topic)
            .and_then(|t| t.partitions.iter().find(|p| p.partition == partition))
            .map(|p| p.last_sequence)
    }
}

/// Where the consumer wants the broker to begin emitting for an assigned
/// `(topic, partition)`. The integer in [`Position::Exact`] is the
/// last offset the consumer has already processed. The broker computes
/// "emit from offset + 1" server-side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Position {
    /// Last offset the consumer has processed; broker emits from offset + 1.
    Exact(Sequence),
    /// Broker-resolved: emit from the partition's retention floor onwards.
    Earliest,
    /// Broker-resolved: emit only events admitted after this SEEK.
    Latest,
    /// Broker-resolved: seek so delivery begins at the first event whose
    /// `occurred_at` is at or after this instant. Parsed at the API boundary,
    /// so no backend re-parses a string or has to invent behaviour for one it
    /// cannot read.
    At(chrono::DateTime<chrono::Utc>),
}

/// One per-partition seed for the pre-stream SEEK call.
#[derive(Debug, Clone)]
pub struct SeekPosition {
    pub topic: String,
    pub partition: u32,
    pub value: Position,
}

/// Partition assignment returned from a JOIN.
/// The starting cursor is established separately via SEEK.
#[derive(Debug, Clone)]
pub struct AssignedPartition {
    pub topic: String,
    pub partition: u32,
}

/// Response returned from a JOIN.
#[derive(Debug, Clone)]
pub struct SubscriptionAssignment {
    pub subscription_id: SubscriptionId,
    pub topology_version: i64,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub assigned: Vec<AssignedPartition>,
}

/// Request body for a JOIN.
#[derive(Debug, Clone)]
pub struct JoinRequest {
    pub group: ConsumerGroupId,
    /// RFC 9110 User-Agent grammar; ASCII 1-256 bytes.
    pub client_agent: String,
    /// Per-member interests (topic-anchored typed-filter selections per ADR-0005).
    pub interests: Vec<SubscriptionInterest>,
    /// Session TTL, refreshed on each poll/seek. Default PT30S.
    pub session_timeout: Option<Duration>,
}

/// An event received from the broker stream.
///
/// It names no topic: a topic is what the event's `type_id` declares, so the
/// consumer resolves it rather than the broker repeating it on every frame.
#[derive(Debug, Clone)]
pub struct WireEvent {
    pub id: Uuid,
    pub type_id: String,
    pub tenant_id: Uuid,
    pub subject: String,
    pub subject_type: String,
    pub partition: u32,
    pub sequence: Sequence,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    pub sequence_time: chrono::DateTime<chrono::Utc>,
    pub trace_parent: Option<String>,
    pub data: serde_json::Value,
}

/// A per-partition cursor position carried by stream topology/control frames.
///
/// The topic is named rather than indexed, so a position identifies its partition
/// on its own - `partition` alone is ambiguous across the topics one subscription
/// can be assigned.
#[derive(Debug, Clone)]
pub struct PartitionPosition {
    pub topic: GtsInstanceId,
    pub partition: u32,
    /// Session cursor - last processed offset.
    pub offset: Sequence,
    /// Highest offset the broker has scanned for this group/partition.
    pub last_examined: Sequence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCode {
    Progress,
    Terminal,
}

/// One frame on the consumption stream.
// `WireFrame::Event(WireEvent)` stays unboxed to preserve the public stream API shape.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum WireFrame {
    Event(WireEvent),
    Heartbeat {
        at: String,
    },
    Topology {
        topology_version: i64,
        assigned: Vec<PartitionPosition>,
    },
    Control {
        code: ControlCode,
        positions: Vec<PartitionPosition>,
        reason: Option<String>,
    },
}

/// Boxed stream returned by [`EventBrokerApi::stream`].
pub type FrameStream =
    std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<WireFrame, EventBrokerError>> + Send>>;

/// Whether to stop traversal at self-managed tenant boundaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BarrierMode {
    #[default]
    Respect,
    Ignore,
}

/// Tenant hierarchy traversal scope for a subscription interest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TenantTraversalDepth {
    #[default]
    CurrentTenant,
    Descendants(NonZeroU32),
    UnlimitedDescendants,
}

impl TenantTraversalDepth {
    pub fn direct_children() -> Self {
        Self::Descendants(NonZeroU32::new(1).expect("1 is non-zero"))
    }

    pub fn descendants(depth: NonZeroU32) -> Self {
        Self::Descendants(depth)
    }

    pub fn unlimited() -> Self {
        Self::UnlimitedDescendants
    }
}

/// Paired filter engine + expression for a subscription interest.
#[derive(Debug, Clone)]
pub struct Filter {
    pub(crate) engine: String,
    pub(crate) expression: String,
}

impl Filter {
    pub fn new(
        engine: impl Into<String>,
        expression: impl Into<String>,
    ) -> Result<Self, EventBrokerError> {
        let engine = engine.into();
        let expression = expression.into();
        if engine.trim().is_empty() {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: "subscription filter engine must not be empty".to_owned(),
                instance: String::new(),
            });
        }
        if expression.is_empty() || expression.len() > 4096 {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: format!(
                    "subscription filter expression must be 1..=4096 bytes (got {})",
                    expression.len()
                ),
                instance: String::new(),
            });
        }
        Ok(Self { engine, expression })
    }

    pub fn engine(&self) -> &str {
        &self.engine
    }

    pub fn expression(&self) -> &str {
        &self.expression
    }
}

/// One interest entry for a subscription JOIN.
#[derive(Debug, Clone)]
pub struct SubscriptionInterest {
    pub(crate) topic: String,
    pub(crate) tenant_id: Uuid,
    pub(crate) tenant_depth: TenantTraversalDepth,
    pub(crate) barrier_mode: BarrierMode,
    pub(crate) types: Vec<String>,
    pub(crate) filter: Option<Filter>,
}

#[derive(Debug, Default)]
pub struct SubscriptionInterestBuilder {
    topic: Option<String>,
    tenant_id: Option<Uuid>,
    tenant_depth: TenantTraversalDepth,
    barrier_mode: BarrierMode,
    types: Vec<String>,
    filter: Option<Filter>,
}

impl SubscriptionInterest {
    pub fn builder() -> SubscriptionInterestBuilder {
        SubscriptionInterestBuilder::default()
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    pub fn tenant_id(&self) -> Uuid {
        self.tenant_id
    }

    pub fn tenant_depth(&self) -> TenantTraversalDepth {
        self.tenant_depth
    }

    pub fn barrier_mode(&self) -> BarrierMode {
        self.barrier_mode
    }

    pub fn types(&self) -> &[String] {
        &self.types
    }

    pub fn filter(&self) -> Option<&Filter> {
        self.filter.as_ref()
    }
}

impl SubscriptionInterestBuilder {
    pub fn topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    pub fn tenant_id(mut self, tenant_id: Uuid) -> Self {
        self.tenant_id = Some(tenant_id);
        self
    }

    pub fn tenant_depth(mut self, tenant_depth: TenantTraversalDepth) -> Self {
        self.tenant_depth = tenant_depth;
        self
    }

    pub fn barrier_mode(mut self, barrier_mode: BarrierMode) -> Self {
        self.barrier_mode = barrier_mode;
        self
    }

    pub fn types<I, S>(mut self, types: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.types = types.into_iter().map(Into::into).collect();
        self
    }

    pub fn filter(mut self, filter: Filter) -> Self {
        self.filter = Some(filter);
        self
    }

    pub fn build(self) -> Result<SubscriptionInterest, EventBrokerError> {
        let topic = self
            .topic
            .ok_or_else(|| EventBrokerError::InvalidConsumerOptions {
                detail: "subscription interest topic is required".to_owned(),
                instance: String::new(),
            })?;
        let tenant_id = self
            .tenant_id
            .ok_or_else(|| EventBrokerError::InvalidConsumerOptions {
                detail: "subscription interest tenant_id is required".to_owned(),
                instance: String::new(),
            })?;
        if topic.trim().is_empty() {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: "subscription interest topic must not be empty".to_owned(),
                instance: String::new(),
            });
        }
        if self.types.is_empty() || self.types.len() > 32 {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: format!(
                    "subscription interest event types must be 1..=32 entries (got {})",
                    self.types.len()
                ),
                instance: String::new(),
            });
        }
        if self.types.iter().any(|ty| ty.trim().is_empty()) {
            return Err(EventBrokerError::InvalidConsumerOptions {
                detail: "subscription interest event types must not contain empty entries"
                    .to_owned(),
                instance: String::new(),
            });
        }

        Ok(SubscriptionInterest {
            topic,
            tenant_id,
            tenant_depth: self.tenant_depth,
            barrier_mode: self.barrier_mode,
            types: self.types,
            filter: self.filter,
        })
    }
}

/// Opaque backend configuration envelope.
/// `gts_type_id` is a full GTS identifier registered with `types-registry-sdk`.
/// `config` is JSON validated against the GTS type's schema.
#[derive(Debug, Clone)]
pub struct StorageBackendConfig {
    pub gts_type_id: String,
    pub config: serde_json::Value,
}

/// Resolved position returned from a SEEK call, one entry per requested partition.
#[derive(Debug, Clone)]
pub struct SeekResult {
    pub topic: String,
    pub partition: u32,
    pub offset: Sequence,
}

// --- EventBrokerApi - client-facing interface ------------------------------------

/// The Event Broker client interface - one method per broker operation.
///
/// Resolved from `ClientHub`:
/// ```ignore
/// let broker = hub.get::<dyn EventBrokerApi>()?;
/// ```
///
/// Implemented by: the in-process direct backend, the remote HTTP backend,
/// and the mock (`--features test-util`). This boundary is **transport-agnostic** - no
/// HTTP types leak through it, and method docs describe operations, not wire paths.
/// HTTP verbs/paths (and their renames) live solely in `openapi.yaml` and the HTTP
/// backend, the single source of truth.
#[async_trait]
pub trait EventBrokerApi: Send + Sync {
    // -- Producer --------------------------------------------------------------
    /// Register a producer; returns the broker-issued producer id. (On the HTTP
    /// wire the response body field is `id`, not `producer_id`.)
    async fn register_producer(
        &self,
        ctx: &SecurityContext,
        mode: ProducerMode,
        client_agent: &str,
    ) -> Result<ProducerId, EventBrokerError>;

    async fn publish(
        &self,
        ctx: &SecurityContext,
        event: &Event,
    ) -> Result<IngestOutcome, EventBrokerError>;

    /// Persist-confirming publish: awaits the backend durable write and returns
    /// `IngestOutcome::Persisted` on success (vs. `Accepted` for the async path).
    /// A `Duplicate` is still reported as `Duplicate`.
    ///
    /// Default impl falls back to [`publish`] for backends that don't model
    /// persist confirmation; the mock overrides it.
    async fn publish_sync(
        &self,
        ctx: &SecurityContext,
        event: &Event,
    ) -> Result<IngestOutcome, EventBrokerError> {
        self.publish(ctx, event).await
    }

    /// Publish a batch atomically. A batch is all-or-nothing (a single
    /// mode-shape or chain violation rejects the whole batch, per DESIGN), so
    /// the outcome is one value for the batch, not one per event.
    async fn publish_batch(
        &self,
        ctx: &SecurityContext,
        events: &[Event],
    ) -> Result<IngestOutcome, EventBrokerError>;

    async fn get_producer_cursors(
        &self,
        ctx: &SecurityContext,
        producer_id: ProducerId,
    ) -> Result<ProducerCursors, EventBrokerError>;

    async fn reset_producer_chain(
        &self,
        ctx: &SecurityContext,
        producer_id: ProducerId,
        scope: ResetScope<'_>,
    ) -> Result<(), EventBrokerError>;

    // -- Consumer groups -------------------------------------------------------
    async fn create_consumer_group(
        &self,
        ctx: &SecurityContext,
        req: CreateConsumerGroupRequest,
    ) -> Result<ConsumerGroup, EventBrokerError>;

    async fn get_consumer_group(
        &self,
        ctx: &SecurityContext,
        id: &ConsumerGroupId,
    ) -> Result<ConsumerGroup, EventBrokerError>;

    async fn list_consumer_groups(
        &self,
        ctx: &SecurityContext,
        query: ConsumerGroupQuery,
    ) -> Result<Page<ConsumerGroup>, EventBrokerError>;

    async fn delete_consumer_group(
        &self,
        ctx: &SecurityContext,
        id: &ConsumerGroupId,
    ) -> Result<(), EventBrokerError>;

    // -- Subscriptions ---------------------------------------------------------
    async fn join(
        &self,
        ctx: &SecurityContext,
        req: JoinRequest,
    ) -> Result<SubscriptionAssignment, EventBrokerError>;

    async fn get_subscription(
        &self,
        ctx: &SecurityContext,
        id: SubscriptionId,
    ) -> Result<Subscription, EventBrokerError>;

    async fn list_subscriptions(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<Subscription>, EventBrokerError>;

    async fn leave(
        &self,
        ctx: &SecurityContext,
        id: SubscriptionId,
    ) -> Result<(), EventBrokerError>;

    async fn stream(
        &self,
        ctx: &SecurityContext,
        id: SubscriptionId,
    ) -> Result<FrameStream, EventBrokerError>;

    async fn seek(
        &self,
        ctx: &SecurityContext,
        id: SubscriptionId,
        positions: &[SeekPosition],
    ) -> Result<Vec<SeekResult>, EventBrokerError>;

    // -- Topic / event-type introspection -------------------------------------
    async fn list_topics(&self, ctx: &SecurityContext) -> Result<Vec<Topic>, EventBrokerError>;

    /// The backend's segment manifest for one `(topic, partition)`. The wire
    /// response is a single manifest object (`docs/openapi.yaml`), not a page.
    async fn list_topic_segments(
        &self,
        ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        range: PartitionRange,
    ) -> Result<TopicSegment, EventBrokerError>;

    async fn list_event_types(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<EventType>, EventBrokerError>;

    async fn get_event_type(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> Result<EventType, EventBrokerError>;
}

// --- EventBrokerBackend - storage plugin seam --------------------------------

/// Plugin trait for swappable storage backends.
///
/// Implemented by the built-in memory and postgres backends; third-party backends
/// register via GTS type extension without modifying broker core.
///
/// **Note:** This trait exposes the public [`Event`](crate::models::Event) envelope.
/// Backend authors need the full shape to persist and assign offsets correctly.
#[async_trait]
pub trait EventBrokerBackend: Send + Sync {
    async fn persist(
        &self,
        ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        events: &[Event],
    ) -> Result<(), StorageBackendError>;

    /// Stored events strictly after `after`, in sequence order, at most
    /// `max_count` of them.
    ///
    /// **Every read of the log is exclusive of the position it names.** Fetching
    /// with sequence 43 returns sequences greater than 43 - never 43 itself, and
    /// not necessarily 44, because the next populated sequence is unknowable in a
    /// space retention and filtering leave sparse. A caller therefore never
    /// computes the position it wants next; it names the one it has consumed.
    /// `Sequence::NONE` reads from the start of the space.
    async fn read(
        &self,
        ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        after: Sequence,
        max_count: usize,
    ) -> Result<Vec<Event>, StorageBackendError>;

    /// Resolves a requested position to the cursor to read after.
    ///
    /// The backend owns this because it is the only thing that knows what it
    /// holds: where retention has reached, what it has assigned, and when each
    /// event occurred.
    ///
    /// - `Exact(n)` returns `n`, or [`StorageBackendError::OffsetOutOfRange`]
    ///   when `n` is not a position a cursor may hold here.
    /// - `Earliest` returns the position just below the oldest event still
    ///   stored, so delivery begins at that event.
    /// - `Latest` returns the highest sequence ever assigned - the highest
    ///   *assigned* rather than the highest still stored, so erasing the newest
    ///   event never moves it backwards.
    /// - `At(t)` returns the position just below the first event whose
    ///   `occurred_at` is at or after `t`, falling back to `Latest` when no
    ///   stored event is that recent.
    ///
    /// A returned position is one a cursor may hold. It is not a claim that an
    /// event occupies it, and frequently none does - the position below the
    /// retention floor is precisely the event retention removed.
    async fn resolve(
        &self,
        ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        position: Position,
    ) -> Result<Sequence, StorageBackendError>;

    async fn query(
        &self,
        ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        range: PartitionRange,
    ) -> Result<Vec<TopicSegment>, StorageBackendError>;

    async fn list_partition_leaders(
        &self,
        ctx: &SecurityContext,
        topic: &str,
    ) -> Result<Vec<PartitionLeader>, StorageBackendError>;

    /// Performs one retention pass over a single partition and reports what it
    /// removed.
    ///
    /// The backend that owns the rows owns keeping them bounded, so this is a
    /// trait method rather than anything the broker does to a backend from
    /// outside - a second backend brings its own enforcement instead of
    /// inheriting the first one's.
    ///
    /// Driven, never self-scheduling: the caller decides the cadence and this
    /// performs exactly one pass. A backend owns no timer and spawns no task,
    /// which is the difference between a test that forces three passes
    /// deterministically and one that sleeps hoping a background thread ran.
    /// It is also why retention still fires for a topic that has stopped
    /// receiving events, which enforcement on the append path would not.
    ///
    /// # Errors
    /// [`StorageBackendError::RetentionFailed`] if the pass could not be
    /// applied. A failed pass removes nothing: whatever it would have removed
    /// is still there for the next one.
    async fn maintain(
        &self,
        ctx: &SecurityContext,
        request: &RetentionRequest,
    ) -> Result<RetentionReport, StorageBackendError>;
}

/// One partition's retention pass: which partition, and the bounds it must end
/// within.
///
/// The duration bound arrives as an absolute instant rather than a duration
/// because the caller owns the clock - it is the thing already ticking. A
/// backend with no clock of its own cannot drift from the broker's, and a test
/// moves the cutoff instead of having to age the events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionRequest {
    topic: String,
    partition: u32,
    oldest_permitted: chrono::DateTime<chrono::Utc>,
    max_stored_bytes: Option<u64>,
}

impl RetentionRequest {
    /// Three arguments of mutually distinguishable types, so none can be passed
    /// in another's place; the optional byte bound is chained.
    #[must_use]
    pub fn for_partition(
        topic: &str,
        partition: u32,
        oldest_permitted: chrono::DateTime<chrono::Utc>,
    ) -> RetentionRequestBuilder {
        RetentionRequestBuilder {
            request: RetentionRequest {
                topic: topic.to_owned(),
                partition,
                oldest_permitted,
                max_stored_bytes: None,
            },
        }
    }

    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    #[must_use]
    pub fn partition(&self) -> u32 {
        self.partition
    }

    /// Events stamped before this instant are past the duration bound.
    #[must_use]
    pub fn oldest_permitted(&self) -> chrono::DateTime<chrono::Utc> {
        self.oldest_permitted
    }

    /// Bytes the partition may hold. `None` leaves it bounded by
    /// [`oldest_permitted`](Self::oldest_permitted) alone, free to grow past
    /// any byte figure.
    #[must_use]
    pub fn max_stored_bytes(&self) -> Option<u64> {
        self.max_stored_bytes
    }
}

pub struct RetentionRequestBuilder {
    request: RetentionRequest,
}

impl RetentionRequestBuilder {
    /// Bounds the partition by stored bytes as well as by age. Whichever bound
    /// is reached first triggers removal.
    #[must_use]
    pub fn max_stored_bytes(mut self, bytes: u64) -> Self {
        self.request.max_stored_bytes = Some(bytes);
        self
    }

    #[must_use]
    pub fn build(self) -> RetentionRequest {
        self.request
    }
}

/// What one retention pass did, and where the partition stands after it.
///
/// Every figure is counted rather than derived: `removed_events` is the rows
/// the pass actually removed and `remaining_events` the rows still stored.
/// Neither is the distance between two sequence numbers - sequences are
/// ordinals, and after a prefix removal that distance is not a count of
/// anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetentionReport {
    /// Events this pass removed.
    pub removed_events: u64,
    /// Stored bytes this pass removed, summed over the rows it removed.
    pub removed_bytes: u64,
    /// Events still stored in the partition.
    pub remaining_events: u64,
    /// Stored bytes those events occupy.
    pub remaining_bytes: u64,
    /// The partition's new floor: the lowest sequence still stored, or `None`
    /// when the pass left the partition empty.
    ///
    /// A reader positioned below this has been overtaken by retention. It is
    /// owed the oldest surviving event and no explanation, so nothing here is
    /// ever reported to a consumer.
    pub oldest_surviving_sequence: Option<Sequence>,
}

/// Builds one backend from an operator's `backend` block.
///
/// The wiring-side counterpart to [`EventBrokerBackend`]: a plugin implements
/// this so the gear can bind a topic to the backend its settings name, without
/// the gear knowing how that backend stores a row. Not a `RunnableCapability`
/// and not a `Gear` - a host builds the provider and injects it, the way the
/// cluster gear injects its cache providers.
#[async_trait]
pub trait EventBrokerBackendProvider: Send + Sync {
    /// The GTS backend type this provider serves, as a topic's `backend.type`
    /// names it - a type derived from `gts.cf.core.events.backend.v1~`.
    ///
    /// A type identifier rather than a short alias because a backend *is* a GTS
    /// type: the plugin registers it, and a running deployment of it is an
    /// instance of that type (`docs/DESIGN.md`, "Backend Type vs. Backend
    /// Instance"). The plugin owns the identifier, so adding a backend adds no
    /// name to the gear.
    fn backend_type(&self) -> &'static str;

    /// Builds the backend from the settings written beside `type` in that
    /// topic's `backend` block.
    ///
    /// The gear passes them through without inspecting them: each backend
    /// publishes its own schema, so the type that can reject an unknown key is
    /// this plugin's, not the gear's. Anything the backend needs that is not
    /// operator configuration - a database handle the host already owns, say -
    /// is captured when the provider itself is constructed, not passed here.
    ///
    /// # Errors
    /// [`StorageBackendError::InvalidConfig`] if `settings` are not the ones
    /// this backend understands.
    async fn build_backend(
        &self,
        settings: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<std::sync::Arc<dyn EventBrokerBackend>, StorageBackendError>;
}
