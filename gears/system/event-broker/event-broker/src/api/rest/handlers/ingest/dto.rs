//! REST DTOs for the ingest-side API (`POST /v1/events`/`:batch`,
//! `/v1/producers`, `/v1/topics`, `/v1/event-types`). These types own serde/
//! `utoipa` schema concerns and convert to/from `domain::model`/
//! `domain::ingest` types via `From`/`TryFrom` impls - the same house
//! convention every other gear's single `api/rest/dto.rs` follows, split
//! into `ingest`/`delivery` here since this crate has two distinct handler
//! groups rather than one.

use gts::GtsTypeId;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ingest::{ProducerCursors, ProducerMode, ProducerResetScope, PublishRequest};
use event_broker_sdk::models::EventType;

use crate::domain::model::{Meta, Sequence, Topic, TopicSegmentManifest};

// ---------------------------------------------------------------------------
// events.rs: POST /v1/events, POST /v1/events:batch
// ---------------------------------------------------------------------------

/// Publish-time transport-metadata block (`gts.cf.core.events.event.v1~.schema.json`'s `meta`) -
/// producer-protocol fields for chained/monotonic modes. Omit entirely for
/// stateless publish.
#[derive(Debug)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct MetaDto {
    pub version: i32,
    pub producer_id: Option<Uuid>,
    pub previous: Option<i64>,
    pub sequence: Option<i64>,
}

/// `POST /v1/events` request body. Deliberately omits `partition`/
/// `sequence`/`sequence_time` (the schema's `readOnly` fields) - a client
/// that supplies them is rejected via `#[serde(deny_unknown_fields)]`
/// (`gts.cf.core.events.event.v1~.schema.json`: "Producers MUST NOT supply this field on
/// publish"). It carries no `topic` either: the stream an event belongs to is
/// the `topic` trait on its event type, so `type` alone names it.
#[derive(Debug)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PublishEventRequest {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub type_id: String,
    pub tenant_id: Uuid,
    pub source: String,
    pub subject: String,
    pub subject_type: String,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub trace_parent: Option<String>,
    #[serde(default)]
    pub data: Option<JsonValue>,
    #[serde(default)]
    pub meta: Option<MetaDto>,
}

impl TryFrom<PublishEventRequest> for PublishRequest {
    type Error = DomainError;

    fn try_from(req: PublishEventRequest) -> Result<Self, DomainError> {
        // Envelope encoding first. The validation pipeline puts schema-level
        // field checks ahead of event-type lookup, so an event whose `source`
        // or `subject` is mis-encoded is rejected for that reason even when its
        // type is also unregistered - a check placed after the lookup could
        // never report on a publish naming an unknown type.
        event_broker_sdk::validate::source(&req.source)?;
        event_broker_sdk::validate::subject(&req.subject)?;

        // A GTS **type** identifier: an event is an instance of its derived
        // event type, so the identifier a publisher sends ends in `~`.
        let r#type = GtsTypeId::try_new(&req.type_id).map_err(|err| DomainError::Validation {
            code: "InvalidBody",
            message: format!("'{}' is not a valid GTS type id: {err}", req.type_id),
        })?;
        Ok(PublishRequest {
            id: req.id,
            r#type,
            tenant_id: req.tenant_id,
            source: req.source,
            subject: req.subject,
            subject_type: req.subject_type,
            occurred_at: req.occurred_at,
            trace_parent: req.trace_parent,
            data: req.data.unwrap_or(JsonValue::Null),
            meta: req.meta.map(|m| Meta {
                version: m.version,
                producer_id: m.producer_id.unwrap_or_default(),
                previous: m.previous.unwrap_or(0),
                sequence: m.sequence.unwrap_or(0),
            }),
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct SyncWaitQuery {
    #[serde(default)]
    pub wait: Option<String>,
}

#[derive(Debug)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PublishBatchRequest {
    pub events: Vec<PublishEventRequest>,
}

// ---------------------------------------------------------------------------
// event_types.rs: GET /v1/event-types
// ---------------------------------------------------------------------------

/// One event type as `GET /v1/event-types` reports it, projected from the
/// registered type schema: `id` is the schema's own identifier, `topic` and
/// `allowed_subject_types` are its resolved trait values, and `data_schema` is
/// the payload contract composed from the `data` narrowings along the
/// derivation chain (`scenarios/topics/1.04-positive-list-event-types.md`).
/// A registration timestamp is not part of that projection - nothing in the
/// schema carries one.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct EventTypeDto {
    pub id: String,
    pub topic: String,
    pub description: Option<String>,
    pub allowed_subject_types: Vec<String>,
    /// JSON Pointer naming the event member the partition is derived from - a
    /// resolved trait of the event type, so it is reported here.
    pub partition_key: String,
    pub data_schema: JsonValue,
}

impl From<EventType> for EventTypeDto {
    fn from(t: EventType) -> Self {
        Self {
            id: t.id.as_ref().to_owned(),
            topic: t.topic.into_string(),
            description: t.description,
            allowed_subject_types: t.allowed_subject_types,
            partition_key: t.partition_key,
            data_schema: t.data_schema,
        }
    }
}

// ---------------------------------------------------------------------------
// producers.rs: POST /v1/producers, GET .../cursors, POST .../{id}:reset
// ---------------------------------------------------------------------------

#[derive(Debug)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RegisterProducerRequest {
    pub mode: ProducerModeDto,
    pub client_agent: String,
}

#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(request, response)]
pub enum ProducerModeDto {
    Chained,
    Monotonic,
}

impl From<ProducerModeDto> for ProducerMode {
    fn from(mode: ProducerModeDto) -> Self {
        match mode {
            ProducerModeDto::Chained => ProducerMode::Chained,
            ProducerModeDto::Monotonic => ProducerMode::Monotonic,
        }
    }
}

impl From<ProducerMode> for ProducerModeDto {
    fn from(mode: ProducerMode) -> Self {
        match mode {
            ProducerMode::Chained => ProducerModeDto::Chained,
            ProducerMode::Monotonic => ProducerModeDto::Monotonic,
        }
    }
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct RegisterProducerResponse {
    pub id: Uuid,
    pub mode: ProducerModeDto,
    pub client_agent: String,
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct ProducerPartitionCursorDto {
    pub partition: i32,
    pub last_sequence: i64,
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct ProducerTopicCursorsDto {
    pub topic: String,
    pub partitions: Vec<ProducerPartitionCursorDto>,
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct ProducerCursorsResponse {
    pub producer_id: Uuid,
    pub client_agent: String,
    pub topics: Vec<ProducerTopicCursorsDto>,
}

impl From<ProducerCursors> for ProducerCursorsResponse {
    fn from(cursors: ProducerCursors) -> Self {
        Self {
            producer_id: cursors.producer_id,
            client_agent: cursors.client_agent,
            topics: cursors
                .topics
                .into_iter()
                .map(|t| ProducerTopicCursorsDto {
                    topic: t.topic,
                    partitions: t
                        .partitions
                        .into_iter()
                        .map(|p| ProducerPartitionCursorDto {
                            partition: p.partition,
                            last_sequence: p.last_sequence,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Default)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct ResetProducerRequest {
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub partition: Option<i32>,
}

impl From<ResetProducerRequest> for ProducerResetScope {
    fn from(req: ResetProducerRequest) -> Self {
        match (req.topic, req.partition) {
            (Some(topic), Some(partition)) => {
                ProducerResetScope::TopicPartition { topic, partition }
            }
            _ => ProducerResetScope::All,
        }
    }
}

// ---------------------------------------------------------------------------
// topics.rs: GET /v1/topics, GET /v1/topics/segments
// ---------------------------------------------------------------------------

/// One topic as `GET /v1/topics` reports it: the registered instance's own
/// values and nothing else (`scenarios/topics/1.01-positive-list-topics.md`).
/// Partition count and streaming settings are broker deployment configuration
/// rather than properties of the stream, so neither is reported here, and the
/// instance carries no registration timestamp to report.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct TopicDto {
    pub id: String,
    pub description: Option<String>,
    pub retention: Option<String>,
}

impl From<Topic> for TopicDto {
    fn from(t: Topic) -> Self {
        Self {
            id: t.id.into_string(),
            description: Some(t.description),
            retention: t.retention.map(|retention| retention.to_string()),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TopicSegmentsQuery {
    pub topic: String,
    pub partition: i32,
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct TopicSegmentsResponse {
    pub topic: String,
    pub partition: i32,
    #[schema(value_type = i64)]
    pub start_sequence: Sequence,
    #[schema(value_type = i64)]
    pub end_sequence: Sequence,
    pub start_time: Option<chrono::DateTime<chrono::Utc>>,
    pub end_time: Option<chrono::DateTime<chrono::Utc>>,
    pub segments: Vec<JsonValue>,
}

impl From<TopicSegmentManifest> for TopicSegmentsResponse {
    fn from(m: TopicSegmentManifest) -> Self {
        Self {
            topic: m.topic.into_string(),
            partition: m.partition,
            start_sequence: m.start_sequence,
            end_sequence: m.end_sequence,
            start_time: m.start_time,
            end_time: m.end_time,
            segments: m.segments,
        }
    }
}
