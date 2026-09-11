//! Domain entities (`DESIGN.md` §3.1 Domain Model). Shapes only - no
//! persistence, validation, or transport concerns live here.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::time::Duration;

use chrono::{DateTime, Utc};
use gts::GtsTypeId;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use toolkit::domain_model;
use toolkit_gts::GtsInstanceId;
use toolkit_utils::iso8601_duration::Iso8601Duration;

use crate::domain::resolution::EffectiveSettings;
use uuid::Uuid;

/// A position in the broker's sequence space.
///
/// Sequences are scoped to one `(topic, partition)` and assigned by the
/// storage backend at persist time, monotonically, starting from 1. Sequence 0
/// is therefore never an event sequence, which is exactly what makes it usable
/// as a cursor: the cursor model is last-processed-offset, the broker emits
/// from `cursor + 1`, and cursor 0 means "nothing processed yet". Consequently
/// no value in this space is ever negative - the valid SEEK range
/// `[RF - 1, HWM]` has a floor of 0 because the retention floor is at least 1.
///
/// See `docs/ADR/0001-offset-semantics.md` and `docs/DESIGN.md`'s "Offset
/// Semantics" section.
///
/// The producer chain (`Meta::previous`/`Meta::sequence`) is a *different*
/// numbering space - producer-assigned per `(producer_id, topic, partition)`
/// for ingest dedup - and deliberately does not use this type.
pub use event_broker_sdk::Sequence;

/// `gts.cf.core.events.topic.v1~` - a named, partitioned event log, as the
/// broker holds it: what the registered instance says, and what this deployment
/// resolved for it.
///
/// The first three fields are the instance's own data, projected by
/// [`crate::domain::projection::topic`]. `settings` is what that projection and
/// configuration resolved to together
/// ([`crate::domain::resolution::resolve`]), so a caller needing a partition
/// count, a retention bound or a backend has one place to look and no second
/// source to reconcile it against.
///
/// An event type has no counterpart here: its flat shape is the projection
/// `event_broker_sdk::models::EventType` already owns, and a second copy in the
/// domain would be a shape to keep in step for no gain.
#[domain_model]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topic {
    pub id: GtsInstanceId,
    /// Required on the instance, so always present on a projected topic.
    pub description: String,
    /// What the topic itself declares, which is advisory: it is one tier of the
    /// retention ladder, and `settings` carries the value that won.
    #[serde(default)]
    pub retention: Option<Iso8601Duration>,
    pub settings: EffectiveSettings,
}

/// `gts.cf.core.events.event.v1~` - an immutable record in a
/// `(topic, partition)` log.
///
/// `Serialize`/`Deserialize` (unlike `event_broker_sdk::models::Event`,
/// which deliberately has none) - this is the exact type
/// `IngestServiceImpl::publish_event` serializes as the ingest outbox
/// payload (design.md D5): reusing this domain type directly means the
/// leased handler that later decodes it can call the existing
/// `domain::backend::to_sdk_event` conversion unchanged, instead of a
/// second, hand-mirrored envelope type duplicating every field.
#[domain_model]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    /// The event type this event is an instance of: a GTS **type** identifier,
    /// ending in `~`, because a concrete event type is a derived type schema
    /// rather than an instance of one.
    pub r#type: GtsTypeId,
    /// Broker-resolved, never producer-supplied: a publish body names no
    /// topic, and `IngestService::publish_event` stamps the one its `type`
    /// resolves to. Present here so the outbox payload and every read-side
    /// consumer of an `Event` can name the log without re-resolving it.
    pub topic: GtsInstanceId,
    pub tenant_id: Uuid,
    pub source: String,
    pub subject: String,
    pub subject_type: String,
    pub occurred_at: DateTime<Utc>,
    pub trace_parent: Option<String>,
    pub data: JsonValue,
    /// Publish-input only; stripped on the read projection.
    pub meta: Option<Meta>,
    /// Read-projection only; broker-derived.
    pub partition: Option<i32>,
    /// Read-projection only.
    pub sequence: Option<Sequence>,
    pub sequence_time: Option<DateTime<Utc>>,
}

/// Producer chain metadata. Publish-input only; stripped on read.
#[domain_model]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub version: i32,
    pub producer_id: Uuid,
    pub previous: i64,
    pub sequence: i64,
}

/// Engine-typed filter expression on one interest (`ADR-0005`).
#[domain_model]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterSpec {
    pub engine: String,
    pub expression: String,
}

/// One topic-anchored interest within a JOIN request/response
/// (`gts.cf.core.events.subscription.v1~.schema.json`'s `Interest` definition): event-type
/// patterns (GTS wildcard rules) scoped to `topic`, with an optional
/// compiled filter. Lives here (not `domain/delivery.rs`) because it's part
/// of `Subscription`'s own persisted/echoed shape, not just a
/// `DeliveryService::join` operation parameter.
/// Whether tenant-hierarchy traversal stops at `self_managed = true` tenant
/// boundaries. `Respect` is the default and the only mode ordinary callers may
/// ask for; `Ignore` traverses through barriers and is platform-services only.
/// Mirrors `tenant_resolver_sdk::BarrierMode`.
#[domain_model]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BarrierMode {
    #[default]
    Respect,
    Ignore,
}

impl BarrierMode {
    /// The wire token, matching this type's `serde` renaming - kept here so
    /// the serialized form and the hand-built response body cannot drift.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Respect => "respect",
            Self::Ignore => "ignore",
        }
    }

    /// Inverse of [`Self::as_wire`]; `None` for any token the schema's enum
    /// does not list, so an unrecognised mode is rejected rather than
    /// defaulted.
    #[must_use]
    pub fn from_wire(token: &str) -> Option<Self> {
        match token {
            "respect" => Some(Self::Respect),
            "ignore" => Some(Self::Ignore),
            _ => None,
        }
    }
}

/// How far below `Interest::tenant_id` a subscription's topic visibility
/// reaches, bounded by [`BarrierMode`]. Aligned with
/// `tenant_resolver_sdk::GetDescendantsOptions::max_depth`.
///
/// An enum rather than the wire's `max_depth: integer | null` because that
/// shape overloads one field with three meanings - `0` for this tenant only,
/// `n` for `n` levels of descendants, `null` for unbounded - and an
/// `Option<u32>` in the domain would leave "unspecified" and "unlimited"
/// indistinguishable at every use site.
#[domain_model]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TenantTraversalDepth {
    #[default]
    CurrentTenant,
    Descendants(NonZeroU32),
    UnlimitedDescendants,
}

impl TenantTraversalDepth {
    /// Reconstructs the depth from the wire's tri-state `max_depth`: absent or
    /// `Some(0)` is the current tenant, `Some(n)` is `n` levels, `None` (an
    /// explicit JSON `null`) is unbounded.
    #[must_use]
    pub fn from_max_depth(max_depth: Option<u32>) -> Self {
        match max_depth {
            None => Self::UnlimitedDescendants,
            Some(0) => Self::CurrentTenant,
            Some(depth) => NonZeroU32::new(depth).map_or(Self::CurrentTenant, Self::Descendants),
        }
    }

    /// The wire's `max_depth`, inverse of [`Self::from_max_depth`].
    #[must_use]
    pub fn max_depth(self) -> Option<u32> {
        match self {
            Self::CurrentTenant => Some(0),
            Self::Descendants(depth) => Some(depth.get()),
            Self::UnlimitedDescendants => None,
        }
    }
}

/// Round-trips through the wire's `max_depth` so a stored `Subscription` and a
/// request body share one representation.
impl Serialize for TenantTraversalDepth {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.max_depth().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TenantTraversalDepth {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_max_depth(Option::<u32>::deserialize(
            deserializer,
        )?))
    }
}

#[domain_model]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interest {
    pub topic: GtsInstanceId,
    /// Authz-validated against the calling principal
    /// (`DeliveryServiceImpl::join`'s `tenant_authorized` call,
    /// `gears-rust#4516`) - a dedicated `authz-resolver` PEP check
    /// (`domain::authz::TENANT_SCOPE_RESOURCE`), not a `tenant-resolver-sdk`
    /// call.
    pub tenant_id: Uuid,
    /// Topic visibility below `tenant_id`. Carried and echoed, but not yet
    /// expanded into a concrete tenant set - fan-out currently matches
    /// `tenant_id` alone, so a depth other than `CurrentTenant` widens
    /// nothing.
    #[serde(default, rename = "max_depth")]
    pub depth: TenantTraversalDepth,
    /// Bounds `depth` at self-managed tenant boundaries. Inert for the same
    /// reason `depth` is.
    #[serde(default)]
    pub barrier_mode: BarrierMode,
    /// GTS **wildcard patterns** (GTS §10 rules, `validate_type_pattern`),
    /// not concrete instance ids - stays `String` deliberately;
    /// `GtsInstanceId::try_new` rejects wildcard segments.
    pub types: Vec<String>,
    pub filter: Option<FilterSpec>,
}

/// `gts.cf.core.events.subscription.v1~` - ephemeral, in-cache consumer
/// instance.
#[domain_model]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub consumer_group: GtsInstanceId,
    pub client_agent: String,
    pub interests: Vec<Interest>,
    pub topics: Vec<GtsInstanceId>,
    pub assigned: Vec<Assignment>,
    /// Monotonically increasing per group; set by `ConsumerGroupCoordinator`
    /// on each JOIN or LEAVE that changes the partition assignment.
    #[serde(default)]
    pub topology_version: i64,
    /// `std::time::Duration` has no built-in `serde` impl - `Storage`'s
    /// `subscription` namespace (eb-single-process-implementation D2)
    /// stores this as JSON in `ClusterCacheV1`, so it round-trips through
    /// whole seconds via `serde_duration_secs` (a pure serialization helper,
    /// not an infra dependency - stays in `domain/` alongside the type that
    /// needs it).
    #[serde(with = "serde_duration_secs")]
    pub session_timeout: Duration,
    pub last_seen_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[domain_model]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Assignment {
    pub topic: GtsInstanceId,
    pub partition: i32,
    pub offset: Sequence,
    pub last_examined: Sequence,
}

/// Ephemeral, in-cache runtime state of a consumer group.
#[domain_model]
#[derive(Debug, Clone)]
pub struct GroupState {
    pub consumer_group: GtsInstanceId,
    pub topic: GtsInstanceId,
    pub per_member_filters: HashMap<Uuid, JsonValue>,
    pub active_members: HashMap<Uuid, Subscription>,
    pub topology_version: i64,
    pub owning_delivery_shard_id: String,
}

/// Ephemeral, in-cache group progress for one `(topic, partition)`.
#[domain_model]
#[derive(Debug, Clone)]
pub struct Cursor {
    pub topic: GtsInstanceId,
    pub consumer_group: GtsInstanceId,
    pub partition: i32,
    pub offset: Sequence,
}

/// Whether a `ConsumerGroup` was minted anonymously by `POST
/// /v1/consumer-groups` or pre-registered by name via the types registry
/// (`docs/openapi.yaml`'s `POST /v1/consumer-groups` description).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerGroupKind {
    Anonymous,
    Named,
}

/// `gts.cf.core.events.consumer_group.v1~` - a registered consumer group.
/// Membership (`GroupState::active_members`) is tracked separately, in
/// cache, not here. `tenant_id`/`owner_principal_id` are non-overridable -
/// captured from `SecurityContext` at create time, never from the request
/// body (`docs/schemas/gts.cf.core.events.consumer_group.v1~.schema.json`).
#[domain_model]
#[derive(Debug, Clone)]
pub struct ConsumerGroup {
    pub id: GtsInstanceId,
    pub kind: ConsumerGroupKind,
    pub tenant_id: Uuid,
    pub owner_principal_id: Uuid,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Caller-suppliable fields for `DeliveryService::create_consumer_group`
/// (`docs/openapi.yaml`'s `POST /v1/consumer-groups`'s `requestBody` is
/// itself optional; `client_agent` is required only once a body is sent at
/// all - both fields are `None` when no body is sent).
#[domain_model]
#[derive(Debug, Clone, Default)]
pub struct ConsumerGroupCreateInput {
    /// Diagnostic hint surfaced in operational logs only - no broker-side
    /// semantic, not persisted on `ConsumerGroup`
    /// (`gts.cf.core.events.consumer_group.v1~.schema.json`'s `CreateRequest.client_agent` doc
    /// comment).
    pub client_agent: Option<String>,
    pub description: Option<String>,
}

/// Backend segment manifest for one `(topic, partition)`
/// (`GET /v1/topics/segments`). `segments` entries are opaque per
/// `docs/openapi.yaml` - this in-memory backing reports the whole stored
/// range as one synthetic segment, which is honest for what it actually is
/// (no real segmented storage backend exists yet - `#4347`/`#4348`).
#[domain_model]
#[derive(Debug, Clone)]
pub struct TopicSegmentManifest {
    pub topic: GtsInstanceId,
    pub partition: i32,
    pub start_sequence: Sequence,
    pub end_sequence: Sequence,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub segments: Vec<JsonValue>,
}

/// `#[serde(with = "...")]` helper for `Subscription::session_timeout` -
/// `std::time::Duration` has no built-in `serde` support. Whole-second
/// precision only (matches every other duration-as-config-field in this
/// crate, e.g. `config::StreamingConfig::heartbeat_interval_secs`).
mod serde_duration_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(value.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(deserializer)?))
    }
}
