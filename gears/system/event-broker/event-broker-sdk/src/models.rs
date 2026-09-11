use crate::sequence::Sequence;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use gts::{GtsInstanceId, GtsTypeId};
use toolkit_utils::iso8601_duration::Iso8601Duration;

use crate::api::SubscriptionInterest;
use crate::error::EventBrokerError;
use crate::ids::ConsumerGroupId;

/// A topic as the broker's API reports it.
///
/// A topic is an instance of the topic base type, so every field here is the
/// instance's own data. How many partitions the broker gives the topic and which
/// backend stores them are the broker's own configuration and are not reported
/// here.
#[derive(Debug, Clone)]
pub struct Topic {
    pub id: GtsInstanceId,
    /// Required on the topic instance, so always present on a projected topic.
    pub description: String,
    pub retention: Option<Iso8601Duration>,
}

/// An event type as the broker's API reports it.
///
/// Projected from the event type's resolved type schema. `data_schema` is the
/// payload contract composed out of the schema's `data` narrowings, and
/// `topic`, `allowed_subject_types` and `partition_key` are resolved traits.
#[derive(Debug, Clone)]
pub struct EventType {
    pub id: GtsTypeId,
    pub topic: GtsInstanceId,
    pub description: Option<String>,
    pub allowed_subject_types: Vec<String>,
    /// JSON Pointer into an event naming the member its partition is derived
    /// from. Resolved from the type's trait, which the base defaults, so every
    /// event type reports one.
    pub partition_key: String,
    pub data_schema: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ConsumerGroup {
    pub id: ConsumerGroupId,
    pub tenant_id: Uuid,
    pub owner_principal_id: Uuid,
    pub kind: ConsumerGroupKind,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerGroupKind {
    Named,
    Anonymous,
}

#[derive(Debug, Clone)]
pub struct Subscription {
    pub id: crate::ids::SubscriptionId,
    pub consumer_group: ConsumerGroupId,
    /// RFC 9110 User-Agent grammar; ASCII 1-256 bytes. Required on the wire
    /// (subscription schema); informational.
    pub client_agent: String,
    /// The member's declared interests, echoed back on the subscription.
    pub interests: Vec<SubscriptionInterest>,
    pub assigned: Vec<PartitionAssignment>,
    pub topology_version: i64,
    pub expires_at: DateTime<Utc>,
}

/// One `(topic, partition)` pair a subscription owns. The topic is named rather
/// than indexed, matching what the subscription's schema declares and what the
/// `topology` and `control` frames carry, so an assignment is readable on its own.
#[derive(Debug, Clone)]
pub struct PartitionAssignment {
    pub topic: GtsInstanceId,
    pub partition: u32,
}

#[derive(Debug, Clone)]
pub struct CreateConsumerGroupRequest {
    /// RFC 9110 User-Agent grammar; ASCII 1-256 bytes. Diagnostic only - no broker semantic.
    pub client_agent: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct PartitionRange {
    pub start_offset: Option<Sequence>,
    pub end_offset: Option<Sequence>,
    pub limit: u32,
}

#[derive(Debug, Clone)]
pub struct TopicSegment {
    pub topic: String,
    pub partition: u32,
    pub start_sequence: Sequence,
    pub end_sequence: Sequence,
    /// Optional on the wire: an empty partition has no span to report.
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    /// Backend-specific per-segment opaque entries. Required in the wire response envelope.
    pub segments: Vec<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct PartitionLeader {
    pub partition: u32,
    pub endpoint: String,
}

/// Paginated result wrapper used by list endpoints (e.g. GET /v1/consumer-groups).
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub prev_cursor: Option<String>,
    pub limit: u32,
}

/// Query parameters for [`EventBrokerApi::list_consumer_groups`](crate::api::EventBrokerApi::list_consumer_groups).
/// Built fluently; `ConsumerGroupQuery::default()` requests the first page with the
/// broker's default limit and no filter/order.
///
/// ```ignore
/// let q = ConsumerGroupQuery::new().limit(50).filter("name eq 'orders'");
/// ```
#[derive(Debug, Clone, Default)]
pub struct ConsumerGroupQuery {
    /// Max items per page (broker default when unset).
    pub limit: Option<u32>,
    /// Opaque pagination cursor from a previous page's `next_cursor`.
    pub cursor: Option<String>,
    /// Filter expression (backend-defined grammar).
    pub filter: Option<String>,
    /// Ordering expression (backend-defined grammar).
    pub orderby: Option<String>,
}

impl ConsumerGroupQuery {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    #[must_use]
    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }
    #[must_use]
    pub fn cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }
    #[must_use]
    pub fn filter(mut self, filter: impl Into<String>) -> Self {
        self.filter = Some(filter.into());
        self
    }
    #[must_use]
    pub fn orderby(mut self, orderby: impl Into<String>) -> Self {
        self.orderby = Some(orderby.into());
        self
    }
}

/// Scope of a producer chain reset
/// ([`EventBrokerApi::reset_producer_chain`](crate::api::EventBrokerApi::reset_producer_chain)).
/// Models the valid combinations directly - a partition reset always names its topic,
/// so "partition without topic" is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetScope<'a> {
    /// Reset every (topic, partition) chain for the producer.
    AllTopics,
    /// Reset every partition chain under one topic.
    Topic(&'a str),
    /// Reset a single (topic, partition) chain.
    Partition { topic: &'a str, partition: u32 },
}

/// The event envelope. Matches `gts.cf.core.events.event.v1~.schema.json` in the design and is the
/// parameter/return type on the public [`EventBrokerApi`](crate::api::EventBrokerApi)
/// (publish/storage side). Broker-stamped fields (`partition`, `sequence`,
/// `sequence_time`) are `None` on publish payloads; the broker populates them on
/// receipt. `sequence` is the only ordering key the wire carries - there is no
/// separate `offset` on an event; a consumer's offset is the `sequence` it has
/// processed.
///
/// This is a plain domain type with no serde derives - construct it via field
/// init. Wire (de)serialization is the transport's concern: the `outbox` async
/// producer round-trips it through its own `OutboxEvent` DTO, and an HTTP backend
/// owns its own wire mapping.
#[derive(Debug, Clone)]
pub struct Event {
    pub id: Uuid,
    pub type_id: String,
    pub tenant_id: Uuid,
    pub source: String,
    pub subject: String,
    pub subject_type: String,
    pub occurred_at: DateTime<Utc>,
    pub trace_parent: Option<String>,
    pub data: Option<serde_json::Value>,

    // Broker-stamped (readOnly on the wire; absent on publish)
    pub partition: Option<u32>,
    pub sequence: Option<Sequence>,
    pub sequence_time: Option<DateTime<Utc>>,

    // Publisher-only (writeOnly; stripped on read)
    pub meta: Option<ProducerMeta>,
}

impl Event {
    /// Resolves an event type's partition-key JSON Pointer against this event,
    /// yielding the value to hash for partition selection.
    ///
    /// A JSON string resolves to its contents; any other scalar to its JSON form,
    /// so a numeric or boolean member is still hashable. The broker checks at
    /// event-type registration that the pointer names a declared member, so a
    /// pointer resolving to nothing here means the event omitted an optional one.
    ///
    /// # Errors
    /// [`EventBrokerError::Internal`] when the pointer resolves to nothing, to
    /// null, or to a container rather than a scalar.
    pub fn partition_input(&self, pointer: &str) -> Result<String, EventBrokerError> {
        let unusable = |detail: &str| {
            EventBrokerError::Internal(format!("partition-key pointer `{pointer}` {detail}"))
        };
        match self
            .addressable()
            .pointer(pointer)
            .ok_or_else(|| unusable("resolves to no member of the event"))?
        {
            serde_json::Value::String(text) => Ok(text.clone()),
            serde_json::Value::Null => Err(unusable("resolves to null")),
            value @ (serde_json::Value::Number(_) | serde_json::Value::Bool(_)) => {
                Ok(value.to_string())
            }
            _ => Err(unusable(
                "resolves to an object or array, which has no stable hash input",
            )),
        }
    }

    /// The event as its base schema declares it.
    ///
    /// [`Event`] carries no serde derives, and a pointer addresses schema member
    /// names rather than Rust field names, so the mapping lives here - one home
    /// for it, shared by the producer computing a partition locally and by a
    /// broker deriving one at ingest. Only publish-time members appear: a pointer
    /// into a server-stamped one could never resolve on the way in.
    fn addressable(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "type": self.type_id,
            "tenant_id": self.tenant_id,
            "source": self.source,
            "subject": self.subject,
            "subject_type": self.subject_type,
            "occurred_at": self.occurred_at,
            "trace_parent": self.trace_parent,
            "data": self.data,
        })
    }
}

/// Publisher-only chain/idempotency metadata stamped onto an [`Event`] before
/// publish (`writeOnly`; the broker strips it on read).
#[derive(Debug, Clone)]
pub struct ProducerMeta {
    pub version: u8,
    pub producer_id: Option<uuid::Uuid>,
    pub previous: Option<i64>,
    pub sequence: Option<i64>,
    pub partition_hint: Option<u32>,
}
