//! Private serde wire DTOs mirroring the event-broker REST bodies.
//!
//! These types are the transport's own; they carry serde so the public SDK
//! models do not. Field names, renames, `deny_unknown_fields`, the `kind`-tagged
//! frame enum and the untagged `max_depth`/`seek value` shapes reproduce the
//! server DTOs in `event-broker/src/api/rest/handlers/{ingest,delivery}/dto.rs`
//! exactly (the server applies `#[serde(rename_all = "snake_case")]` via
//! `#[toolkit_macros::api_dto]`, matched here). Sequences travel as bare `i64`
//! and timestamps as RFC 3339, so nothing here depends on a public SDK type's
//! representation.
//!
//! Only the transport speaks these types; none is re-exported from the crate.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use uuid::Uuid;

use crate::api::{ControlCode, PartitionPosition, WireEvent, WireFrame};
use crate::error::EventBrokerError;
use crate::sequence::Sequence;

// ---------------------------------------------------------------------------
// Pagination envelope (toolkit_odata::Page) - `{ items, page_info }`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PageWire<T> {
    pub items: Vec<T>,
    pub page_info: PageInfoWire,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PageInfoWire {
    pub next_cursor: Option<String>,
    pub prev_cursor: Option<String>,
    pub limit: u64,
}

// ---------------------------------------------------------------------------
// Ingest: POST /v1/events, /v1/events:batch
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct MetaWire {
    pub version: i32,
    pub producer_id: Option<Uuid>,
    pub previous: Option<i64>,
    pub sequence: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct PublishEventWire {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub type_id: String,
    pub tenant_id: Uuid,
    pub source: String,
    pub subject: String,
    pub subject_type: String,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<MetaWire>,
}

impl PublishEventWire {
    /// Builds the publish body from a public [`Event`](crate::models::Event).
    /// Broker-stamped fields (`partition`/`sequence`/...) are never sent - the
    /// server rejects them via `deny_unknown_fields`.
    pub(crate) fn from_event(event: &crate::models::Event) -> Self {
        Self {
            id: event.id,
            type_id: event.type_id.clone(),
            tenant_id: event.tenant_id,
            source: event.source.clone(),
            subject: event.subject.clone(),
            subject_type: event.subject_type.clone(),
            occurred_at: event.occurred_at,
            trace_parent: event.trace_parent.clone(),
            data: event.data.clone(),
            meta: event.meta.as_ref().map(|m| MetaWire {
                version: i32::from(m.version),
                producer_id: m.producer_id,
                previous: m.previous,
                sequence: m.sequence,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct PublishBatchWire {
    pub events: Vec<PublishEventWire>,
}

// ---------------------------------------------------------------------------
// Ingest: producers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProducerModeWire {
    Chained,
    Monotonic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct RegisterProducerWire {
    pub mode: ProducerModeWire,
    pub client_agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct RegisterProducerResponseWire {
    pub id: Uuid,
    pub mode: ProducerModeWire,
    pub client_agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ProducerPartitionCursorWire {
    pub partition: i32,
    pub last_sequence: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ProducerTopicCursorsWire {
    pub topic: String,
    pub partitions: Vec<ProducerPartitionCursorWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ProducerCursorsResponseWire {
    pub producer_id: Uuid,
    pub client_agent: String,
    pub topics: Vec<ProducerTopicCursorsWire>,
}

impl From<ProducerCursorsResponseWire> for crate::api::ProducerCursors {
    fn from(w: ProducerCursorsResponseWire) -> Self {
        Self {
            producer_id: crate::ids::ProducerId(w.producer_id),
            client_agent: w.client_agent,
            topics: w
                .topics
                .into_iter()
                .map(|t| crate::api::TopicCursors {
                    topic: t.topic,
                    partitions: t
                        .partitions
                        .into_iter()
                        .map(|p| crate::api::PartitionCursor {
                            partition: u32::try_from(p.partition).unwrap_or_default(),
                            last_sequence: p.last_sequence,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct ResetProducerWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<i32>,
}

impl ResetProducerWire {
    pub(crate) fn from_scope(scope: crate::models::ResetScope<'_>) -> Self {
        match scope {
            crate::models::ResetScope::AllTopics => Self::default(),
            crate::models::ResetScope::Topic(topic) => Self {
                topic: Some(topic.to_owned()),
                partition: None,
            },
            crate::models::ResetScope::Partition { topic, partition } => Self {
                topic: Some(topic.to_owned()),
                partition: Some(i32::try_from(partition).unwrap_or(i32::MAX)),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Ingest: event types, topics, segments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct EventTypeWire {
    pub id: String,
    pub topic: String,
    pub description: Option<String>,
    pub allowed_subject_types: Vec<String>,
    pub partition_key: String,
    pub data_schema: JsonValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct TopicWire {
    pub id: String,
    pub description: Option<String>,
    pub retention: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct TopicSegmentsResponseWire {
    pub topic: String,
    pub partition: i32,
    pub start_sequence: i64,
    pub end_sequence: i64,
    pub start_time: Option<chrono::DateTime<chrono::Utc>>,
    pub end_time: Option<chrono::DateTime<chrono::Utc>>,
    pub segments: Vec<JsonValue>,
}

// ---------------------------------------------------------------------------
// Delivery: consumer groups
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct CreateConsumerGroupWire {
    pub client_agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl CreateConsumerGroupWire {
    pub(crate) fn from_request(req: crate::models::CreateConsumerGroupRequest) -> Self {
        Self {
            client_agent: req.client_agent,
            description: req.description,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConsumerGroupKindWire {
    Anonymous,
    Named,
}

impl From<ConsumerGroupKindWire> for crate::models::ConsumerGroupKind {
    fn from(k: ConsumerGroupKindWire) -> Self {
        match k {
            ConsumerGroupKindWire::Anonymous => Self::Anonymous,
            ConsumerGroupKindWire::Named => Self::Named,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ConsumerGroupWire {
    pub id: String,
    pub kind: ConsumerGroupKindWire,
    pub tenant_id: Uuid,
    pub owner_principal_id: Uuid,
    pub description: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// ---------------------------------------------------------------------------
// Delivery: subscriptions (JOIN / read / list)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct FilterSpecWire {
    pub engine: String,
    pub expression: String,
}

/// Untagged tri-state: an integer depth, or an explicit JSON `null` for
/// unbounded. Matches the server's `MaxDepthDto` (whose `#[serde(default)]` is
/// `Levels(0)` - current tenant only).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum MaxDepthWire {
    Unlimited,
    Levels(i32),
}

impl Default for MaxDepthWire {
    fn default() -> Self {
        Self::Levels(0)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BarrierModeWire {
    #[default]
    Respect,
    Ignore,
}

impl From<crate::api::BarrierMode> for BarrierModeWire {
    fn from(m: crate::api::BarrierMode) -> Self {
        match m {
            crate::api::BarrierMode::Respect => Self::Respect,
            crate::api::BarrierMode::Ignore => Self::Ignore,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct InterestWire {
    pub topic: String,
    pub tenant_id: Uuid,
    #[serde(default)]
    pub max_depth: MaxDepthWire,
    #[serde(default)]
    pub barrier_mode: BarrierModeWire,
    pub types: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<FilterSpecWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct JoinSubscriptionWire {
    pub consumer_group: String,
    pub client_agent: String,
    pub interests: Vec<InterestWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_timeout: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct FilterSpecResponseWire {
    pub engine: String,
    pub expression: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct InterestResponseWire {
    pub topic: String,
    pub tenant_id: Uuid,
    pub max_depth: Option<i32>,
    pub barrier_mode: String,
    pub types: Vec<String>,
    pub filter: Option<FilterSpecResponseWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct AssignedPartitionWire {
    pub topic: String,
    pub partition: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct SubscriptionWire {
    pub id: Uuid,
    pub consumer_group: String,
    pub client_agent: String,
    pub interests: Vec<InterestResponseWire>,
    pub assigned: Vec<AssignedPartitionWire>,
    pub topology_version: i64,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

// ---------------------------------------------------------------------------
// Delivery: seek
// ---------------------------------------------------------------------------

/// Untagged: an exact integer offset, or a sentinel string
/// (`earliest`/`latest`/`at:<rfc3339>`). Matches the server's `SeekValueDto`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum SeekValueWire {
    Exact(i64),
    Sentinel(String),
}

impl From<&crate::api::Position> for SeekValueWire {
    fn from(p: &crate::api::Position) -> Self {
        match p {
            crate::api::Position::Exact(seq) => Self::Exact(seq.as_i64()),
            crate::api::Position::Earliest => Self::Sentinel("earliest".to_owned()),
            crate::api::Position::Latest => Self::Sentinel("latest".to_owned()),
            crate::api::Position::At(at) => Self::Sentinel(format!("at:{}", at.to_rfc3339())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct PartitionPositionWire {
    pub topic: String,
    pub partition: i32,
    pub value: SeekValueWire,
}

impl From<&crate::api::SeekPosition> for PartitionPositionWire {
    fn from(s: &crate::api::SeekPosition) -> Self {
        Self {
            topic: s.topic.clone(),
            partition: i32::try_from(s.partition).unwrap_or(i32::MAX),
            value: SeekValueWire::from(&s.value),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct SeekSubscriptionWire {
    pub partition_positions: Vec<PartitionPositionWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct ResolvedPositionWire {
    pub topic: String,
    pub partition: i32,
    pub value: i64,
}

impl From<ResolvedPositionWire> for crate::api::SeekResult {
    fn from(w: ResolvedPositionWire) -> Self {
        Self {
            topic: w.topic,
            partition: u32::try_from(w.partition).unwrap_or_default(),
            offset: Sequence::assigned(w.value),
        }
    }
}

// ---------------------------------------------------------------------------
// Delivery: stream frames (kind-tagged), mirroring the server `FrameDto`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct EventPayloadWire {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub type_id: String,
    pub topic: String,
    pub tenant_id: Uuid,
    pub source: String,
    pub subject: String,
    pub subject_type: String,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    pub trace_parent: Option<String>,
    pub data: JsonValue,
    pub partition: Option<i32>,
    pub sequence: Option<i64>,
    pub sequence_time: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct PositionWire {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    pub last_examined: i64,
}

impl PositionWire {
    fn into_domain(self) -> Result<PartitionPosition, EventBrokerError> {
        Ok(PartitionPosition {
            topic: gts::GtsInstanceId::try_new(&self.topic).map_err(|err| {
                EventBrokerError::Transport(format!("stream frame carried an invalid topic: {err}"))
            })?,
            partition: u32::try_from(self.partition).unwrap_or_default(),
            offset: Sequence::assigned(self.offset),
            last_examined: Sequence::assigned(self.last_examined),
        })
    }
}

/// One frame as the delivery stream carries it, discriminated by a top-level
/// `kind` field. Both the multipart and SSE framings wrap this same body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum FrameWire {
    Event {
        payload: Box<EventPayloadWire>,
    },
    Heartbeat {
        at: chrono::DateTime<chrono::Utc>,
    },
    Topology {
        topology_version: i64,
        assigned: Vec<PositionWire>,
    },
    Control {
        code: String,
        positions: Vec<PositionWire>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl FrameWire {
    /// Converts a decoded wire frame into the public [`WireFrame`]. Malformed
    /// content (an event missing its sequence, an unknown control code, an
    /// invalid topic) becomes a transport error rather than a silent default.
    pub(crate) fn into_frame(self) -> Result<WireFrame, EventBrokerError> {
        match self {
            FrameWire::Event { payload } => {
                let p = *payload;
                let sequence = p.sequence.ok_or_else(|| {
                    EventBrokerError::Transport("stream event frame carried no sequence".to_owned())
                })?;
                let partition = p.partition.ok_or_else(|| {
                    EventBrokerError::Transport("stream event frame carried no partition".to_owned())
                })?;
                Ok(WireFrame::Event(WireEvent {
                    id: p.id,
                    type_id: p.type_id,
                    tenant_id: p.tenant_id,
                    subject: p.subject,
                    subject_type: p.subject_type,
                    partition: u32::try_from(partition).unwrap_or_default(),
                    sequence: Sequence::assigned(sequence),
                    occurred_at: p.occurred_at,
                    sequence_time: p.sequence_time.unwrap_or(p.occurred_at),
                    trace_parent: p.trace_parent,
                    data: p.data,
                }))
            }
            FrameWire::Heartbeat { at } => Ok(WireFrame::Heartbeat {
                at: at.to_rfc3339(),
            }),
            FrameWire::Topology {
                topology_version,
                assigned,
            } => Ok(WireFrame::Topology {
                topology_version,
                assigned: assigned
                    .into_iter()
                    .map(PositionWire::into_domain)
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            FrameWire::Control {
                code,
                positions,
                reason,
            } => Ok(WireFrame::Control {
                code: match code.as_str() {
                    "progress" => ControlCode::Progress,
                    "terminal" => ControlCode::Terminal,
                    other => {
                        return Err(EventBrokerError::Transport(format!(
                            "stream control frame carried an unknown code: {other}"
                        )));
                    }
                },
                positions: positions
                    .into_iter()
                    .map(PositionWire::into_domain)
                    .collect::<Result<Vec<_>, _>>()?,
                reason,
            }),
        }
    }
}

#[cfg(test)]
mod tests;
