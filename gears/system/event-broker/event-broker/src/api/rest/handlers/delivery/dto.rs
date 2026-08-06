//! REST DTOs for the delivery-side API (consumer groups, subscriptions,
//! streaming). These types own serde/`utoipa` schema concerns and convert
//! to/from `domain::model`/`domain::delivery` types via `From`/`TryFrom`
//! impls - the same house convention every other gear's single
//! `api/rest/dto.rs` follows, split into `ingest`/`delivery` here since this
//! crate has two distinct handler groups rather than one.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use toolkit_gts::GtsInstanceId;
use uuid::Uuid;

use crate::domain::delivery::{ControlCode, Frame, Position};
use crate::domain::error::DomainError;
use crate::domain::model::{
    Assignment, BarrierMode, ConsumerGroup, ConsumerGroupCreateInput, ConsumerGroupKind, Event,
    FilterSpec, Interest, Sequence, Subscription, TenantTraversalDepth,
};

// ---------------------------------------------------------------------------
// consumer_groups.rs: CRUD for consumer groups
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(response)]
pub enum ConsumerGroupKindDto {
    Anonymous,
    Named,
}

impl From<ConsumerGroupKind> for ConsumerGroupKindDto {
    fn from(kind: ConsumerGroupKind) -> Self {
        match kind {
            ConsumerGroupKind::Anonymous => ConsumerGroupKindDto::Anonymous,
            ConsumerGroupKind::Named => ConsumerGroupKindDto::Named,
        }
    }
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
// [todo]: what about TTL for the consumer group? check the DESIGN.md
pub struct ConsumerGroupDto {
    pub id: String,
    pub kind: ConsumerGroupKindDto,
    pub tenant_id: uuid::Uuid,
    pub owner_principal_id: uuid::Uuid,
    pub description: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<ConsumerGroup> for ConsumerGroupDto {
    fn from(g: ConsumerGroup) -> Self {
        Self {
            id: g.id.to_string(),
            kind: g.kind.into(),
            tenant_id: g.tenant_id,
            owner_principal_id: g.owner_principal_id,
            description: g.description,
            created_at: g.created_at,
        }
    }
}

/// Request body for `POST /v1/consumer-groups`
/// (`docs/schemas/gts.cf.core.events.consumer_group.v1~.schema.json#/definitions/CreateRequest`)
/// - the whole body is optional (`docs/openapi.yaml`'s `requestBody.required:
/// false`); `client_agent` is required only once a body is sent at all
/// (`create_consumer_group`'s manual body-bytes handling, matching
/// `ingest::producers::reset_producer`'s established optional-body pattern).
#[derive(Debug)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateConsumerGroupRequest {
    pub client_agent: String,
    #[serde(default)]
    pub description: Option<String>,
}

impl From<CreateConsumerGroupRequest> for ConsumerGroupCreateInput {
    fn from(req: CreateConsumerGroupRequest) -> Self {
        Self {
            client_agent: Some(req.client_agent),
            description: req.description,
        }
    }
}

// ---------------------------------------------------------------------------
// streaming.rs: GET /v1/events:stream, GET /v1/events:sse
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    pub subscription_id: Uuid,
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub(crate) struct EventPayloadDto {
    id: Uuid,
    r#type: String,
    topic: String,
    tenant_id: Uuid,
    source: String,
    subject: String,
    subject_type: String,
    occurred_at: DateTime<Utc>,
    trace_parent: Option<String>,
    data: JsonValue,
    partition: Option<i32>,
    #[schema(value_type = Option<i64>)]
    sequence: Option<Sequence>,
    sequence_time: Option<DateTime<Utc>>,
}

impl From<Event> for EventPayloadDto {
    fn from(e: Event) -> Self {
        Self {
            id: e.id,
            r#type: e.r#type.into_string(),
            topic: e.topic.into_string(),
            tenant_id: e.tenant_id,
            source: e.source,
            subject: e.subject,
            subject_type: e.subject_type,
            occurred_at: e.occurred_at,
            trace_parent: e.trace_parent,
            data: e.data,
            partition: e.partition,
            sequence: e.sequence,
            sequence_time: e.sequence_time,
        }
    }
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub(crate) struct PositionDto {
    topic: String,
    partition: i32,
    #[schema(value_type = i64)]
    offset: Sequence,
    #[schema(value_type = i64)]
    last_examined: Sequence,
}

/// Wire shape for one frame (`event-broker-consumption-frames`): a JSON
/// object with a top-level `kind` discriminant.
#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
#[serde(tag = "kind")]
pub(crate) enum FrameDto {
    Event {
        payload: Box<EventPayloadDto>,
    },
    Heartbeat {
        at: DateTime<Utc>,
    },
    Topology {
        topology_version: i64,
        assigned: Vec<PositionDto>,
    },
    Control {
        code: &'static str,
        positions: Vec<PositionDto>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl From<crate::domain::streaming::frames::Position> for PositionDto {
    fn from(position: crate::domain::streaming::frames::Position) -> Self {
        Self {
            topic: position.topic.into_string(),
            partition: position.partition,
            offset: position.offset,
            last_examined: position.last_examined,
        }
    }
}

impl From<Frame> for FrameDto {
    fn from(frame: Frame) -> Self {
        match frame {
            Frame::Event(event) => FrameDto::Event {
                payload: Box::new((*event).into()),
            },
            Frame::Heartbeat { at } => FrameDto::Heartbeat { at },
            Frame::Topology {
                topology_version,
                positions,
            } => FrameDto::Topology {
                topology_version,
                assigned: positions.into_iter().map(PositionDto::from).collect(),
            },
            Frame::Control {
                code,
                positions,
                reason,
            } => FrameDto::Control {
                code: match code {
                    ControlCode::Progress => "progress",
                    ControlCode::Terminal => "terminal",
                },
                positions: positions.into_iter().map(PositionDto::from).collect(),
                reason: reason.map(|reason| reason.as_wire().to_owned()),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// subscriptions.rs: JOIN, list, read, leave, seek
// ---------------------------------------------------------------------------

#[derive(Debug)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct FilterSpecDto {
    pub engine: String,
    pub expression: String,
}

impl From<FilterSpecDto> for FilterSpec {
    fn from(f: FilterSpecDto) -> Self {
        Self {
            engine: f.engine,
            expression: f.expression,
        }
    }
}

/// Deserializes a present field - `null` included - into `Some(..)`, leaving
/// the outer `None` to mean "the field was absent". Only reached when the
/// field is present, so wrapping unconditionally is what draws the
/// distinction.
/// Wire form of an interest's `max_depth`, whose three states arrive as two
/// JSON shapes: an integer, or an explicit `null` meaning unbounded. Matching
/// that shape with an untagged enum is what keeps an absent field and a `null`
/// distinguishable - a plain `Option<i32>` collapses them, and `serde` maps a
/// `null` onto the outer `None` of a nested `Option` too.
#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(request)]
#[serde(untagged)]
pub enum MaxDepthDto {
    /// An explicit JSON `null`.
    Unlimited,
    /// A concrete depth. `i32` rather than `u32` so a negative value is
    /// rejected by name below, instead of as an untagged "matched no variant".
    #[schema(value_type = i32)]
    Levels(i32),
}

impl Default for MaxDepthDto {
    /// An omitted `max_depth` is the *current tenant only*, per the schema's
    /// `default: 0`. Deliberately not `Unlimited`: that is the widest scope
    /// this field can express, so defaulting to it would silently widen every
    /// request that leaves the field out.
    fn default() -> Self {
        Self::Levels(0)
    }
}

#[derive(Debug, Clone, Copy, Default)]
#[toolkit_macros::api_dto(request)]
pub enum BarrierModeDto {
    #[default]
    Respect,
    Ignore,
}

impl From<BarrierModeDto> for BarrierMode {
    fn from(mode: BarrierModeDto) -> Self {
        match mode {
            BarrierModeDto::Respect => Self::Respect,
            BarrierModeDto::Ignore => Self::Ignore,
        }
    }
}

#[derive(Debug)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct InterestDto {
    pub topic: String,
    pub tenant_id: Uuid,
    #[serde(default)]
    pub max_depth: MaxDepthDto,
    #[serde(default)]
    pub barrier_mode: BarrierModeDto,
    pub types: Vec<String>,
    #[serde(default)]
    pub filter: Option<FilterSpecDto>,
}

impl TryFrom<InterestDto> for Interest {
    type Error = DomainError;

    fn try_from(i: InterestDto) -> Result<Self, DomainError> {
        let topic = GtsInstanceId::try_new(&i.topic).map_err(|err| DomainError::Validation {
            code: "InvalidBody",
            message: format!("'{}' is not a valid GTS instance id: {err}", i.topic),
        })?;
        let depth = match i.max_depth {
            MaxDepthDto::Unlimited => TenantTraversalDepth::UnlimitedDescendants,
            MaxDepthDto::Levels(depth) => {
                let depth = u32::try_from(depth).map_err(|_| DomainError::Validation {
                    code: "InvalidBody",
                    message: format!("'max_depth' must be >= 0 or null, got {depth}"),
                })?;
                TenantTraversalDepth::from_max_depth(Some(depth))
            }
        };
        Ok(Self {
            topic,
            tenant_id: i.tenant_id,
            depth,
            barrier_mode: i.barrier_mode.into(),
            types: i.types,
            filter: i.filter.map(FilterSpec::from),
        })
    }
}

#[derive(Debug)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct JoinSubscriptionRequest {
    pub consumer_group: String,
    pub client_agent: String,
    pub interests: Vec<InterestDto>,
    #[serde(default)]
    pub session_timeout: Option<String>,
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct FilterSpecResponseDto {
    pub engine: String,
    pub expression: String,
}

impl From<FilterSpec> for FilterSpecResponseDto {
    fn from(f: FilterSpec) -> Self {
        Self {
            engine: f.engine,
            expression: f.expression,
        }
    }
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct InterestResponseDto {
    pub topic: String,
    pub tenant_id: Uuid,
    /// Echoed in the wire's own tri-state form: `0` for the current tenant
    /// only, `n` for `n` levels of descendants, `null` for unbounded.
    pub max_depth: Option<i32>,
    pub barrier_mode: String,
    pub types: Vec<String>,
    pub filter: Option<FilterSpecResponseDto>,
}

impl From<Interest> for InterestResponseDto {
    fn from(i: Interest) -> Self {
        Self {
            topic: i.topic.into_string(),
            tenant_id: i.tenant_id,
            // Saturating, not `try_from(..).ok()`: a `None` here would echo
            // `null`, which the wire reads as *unbounded* - the widest
            // possible answer to an out-of-range depth.
            max_depth: i
                .depth
                .max_depth()
                .map(|depth| i32::try_from(depth).unwrap_or(i32::MAX)),
            barrier_mode: i.barrier_mode.as_wire().to_owned(),
            types: i.types,
            filter: i.filter.map(FilterSpecResponseDto::from),
        }
    }
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct AssignedPartitionDto {
    pub topic: String,
    pub partition: i32,
}

impl From<Assignment> for AssignedPartitionDto {
    fn from(a: Assignment) -> Self {
        Self {
            topic: a.topic.into_string(),
            partition: a.partition,
        }
    }
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct SubscriptionDto {
    pub id: Uuid,
    pub consumer_group: String,
    pub client_agent: String,
    pub interests: Vec<InterestResponseDto>,
    pub assigned: Vec<AssignedPartitionDto>,
    pub topology_version: i64,
    pub expires_at: DateTime<Utc>,
}

impl From<Subscription> for SubscriptionDto {
    fn from(s: Subscription) -> Self {
        Self {
            id: s.id,
            consumer_group: s.consumer_group.to_string(),
            client_agent: s.client_agent,
            interests: s
                .interests
                .into_iter()
                .map(InterestResponseDto::from)
                .collect(),
            assigned: s
                .assigned
                .into_iter()
                .map(AssignedPartitionDto::from)
                .collect(),
            topology_version: s.topology_version,
            expires_at: s.expires_at,
        }
    }
}

#[derive(Debug)]
#[toolkit_macros::api_dto(request)]
#[serde(untagged)]
pub enum SeekValueDto {
    #[schema(value_type = i64)]
    Exact(Sequence),
    Sentinel(String),
}

impl SeekValueDto {
    pub(super) fn into_domain(self) -> Result<Position, DomainError> {
        match self {
            SeekValueDto::Exact(v) => Ok(Position::Exact(v)),
            SeekValueDto::Sentinel(s) if s == "earliest" => Ok(Position::Earliest),
            SeekValueDto::Sentinel(s) if s == "latest" => Ok(Position::Latest),
            SeekValueDto::Sentinel(s) => {
                let ts = s
                    .strip_prefix("at:")
                    .ok_or_else(|| DomainError::Validation {
                        code: "InvalidSeekValue",
                        message: format!(
                            "'{s}' is not a valid SEEK value - expected an integer, \"earliest\", \
                         \"latest\", or \"at:<ISO-8601>\""
                        ),
                    })?;
                let parsed =
                    DateTime::parse_from_rfc3339(ts).map_err(|_| DomainError::Validation {
                        code: "InvalidTimestamp",
                        message: format!("'{ts}' is not a valid ISO-8601 timestamp"),
                    })?;
                Ok(Position::At(parsed.with_timezone(&Utc)))
            }
        }
    }
}

#[derive(Debug)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PartitionPositionDto {
    pub topic: String,
    pub partition: i32,
    pub value: SeekValueDto,
}

#[derive(Debug)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct SeekSubscriptionRequest {
    pub partition_positions: Vec<PartitionPositionDto>,
}

#[derive(Debug)]
#[toolkit_macros::api_dto(response)]
pub struct ResolvedPositionDto {
    pub topic: String,
    pub partition: i32,
    #[schema(value_type = i64)]
    pub value: Sequence,
}
