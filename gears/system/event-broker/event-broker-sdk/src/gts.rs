//! GTS type definitions the broker owns.
//!
//! Exactly two base types are declared here, and the broker declares nothing
//! else. A concrete topic is a well-known **instance** of [`TopicV1`]; a
//! concrete event type is a **derived type schema** under [`EventV1`], and a
//! published event is an instance of that derived type. Both are registered in
//! `types-registry` by whichever gear owns them, at that gear's init - never by
//! the broker.
//!
//! These declarations are the source of truth for both base schemas: the
//! process-wide `toolkit-gts` inventory seeds `types-registry` from what the
//! macro emits here, not from `docs/schemas/`. The two files under
//! `docs/schemas/` are generated from this module and held to it by
//! `gts_tests`, so per-field keywords live in the schemars attributes below.

use gts::{GtsInstanceId, GtsTypeId};
use schemars::JsonSchema;
use serde::Serialize;
use toolkit_gts::{GtsTraitsSchema, gts_id, gts_type_schema};
use toolkit_utils::iso8601_duration::Iso8601Duration;
use uuid::Uuid;

/// ASCII-printable, the platform convention for every event field except `data`.
const ASCII_PRINTABLE: &str = r"^[\x20-\x7E]+$";

/// Printable ASCII with an empty value permitted, for a field whose presence is
/// governed by `required` rather than by its own pattern. A `description` is
/// optional on some resources and required on others; the encoding rule is the
/// same either way, and an empty string is a legitimate way to say "none".
const ASCII_PRINTABLE_OR_EMPTY: &str = r"^[\x20-\x7E]*$";

// One `allowed_subject_types` entry: a GTS Type-id pattern naming an entity kind
// whose events may declare it in `subject_type`. `x-gts-type` is `true` rather
// than a prefix because the entity an event is about belongs to another domain,
// so the broker can require a type identifier without naming its family.
#[derive(Serialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent, extend("x-gts-type" = true))]
pub struct SubjectTypeRef(pub String);

/// The default the base declares for [`EventTraits::partition_key`]: the event's
/// tenant. Every event carries one, so the default always resolves.
const TENANT_POINTER: &str = "/tenant_id";

// A JSON Pointer (RFC 6901) into an event, naming the member whose value is
// hashed to pick the partition. `json-pointer` is a registered JSON Schema
// format, so the constraint is one a generic validator can act on.
#[derive(Serialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent, extend("format" = "json-pointer"))]
pub struct PartitionKeyRef(pub String);

// The topic an event type publishes to: an instance of the topic base type, so a
// derived event type naming something that is not a topic is rejected at
// registration rather than at its first publish.
#[derive(Serialize, JsonSchema)]
#[serde(transparent)]
#[schemars(transparent, extend("x-gts-instance" = gts_id!("cf.core.events.topic.v1~")))]
pub struct TopicRef(pub String);

// Traits for `gts.cf.core.events.event.v1~`, emitted as the base's
// `x-gts-traits-schema`. They govern how the broker treats events of a type and
// are schema-only keywords, so no publisher can send them and no event payload
// can carry them. A `///` here would become a `description` on the emitted
// trait-schema object; field `///` docs are wanted, since each becomes that
// trait's `description`.
#[allow(clippy::doc_markdown)]
#[derive(Serialize, JsonSchema, GtsTraitsSchema)]
#[serde(deny_unknown_fields)]
pub struct EventTraits {
    /// Full GTS topic identifier of the stream that events of this type are published to. A topic is an instance, so this never ends in `~`.
    pub topic: TopicRef,
    /// GTS Type-id patterns describing which subject types an event of this type may declare in subject_type (concrete Type match, wildcard suffix <prefix>.*, or bare base Type <base>~ with implicit derived-type coverage). Enforced at publish time, independent of the subject_type:produce/:consume authorization check.
    #[serde(default)]
    pub allowed_subject_types: Vec<SubjectTypeRef>,
    /// JSON Pointer naming the member of an event whose value determines its partition. May point into `data`, which a bare field name could not express. Defaults to the event's tenant, so a type declaring nothing partitions per tenant. The broker checks at registration that the pointer names a member this type's resolved schema declares.
    #[serde(default = "default_partition_key")]
    #[schemars(extend("default" = TENANT_POINTER))]
    pub partition_key: PartitionKeyRef,
}

fn default_partition_key() -> PartitionKeyRef {
    PartitionKeyRef(TENANT_POINTER.to_owned())
}

/// Base topic type. A concrete topic is a well-known instance of it, carrying
/// the stream's own data: what it is called, what it is for, and how long its
/// events are kept. How many partitions the broker gives it and which backend
/// stores them are the broker's concerns, configured there rather than declared
/// per topic.
#[gts_type_schema(
    dir_path = "schemas",
    base = true,
    type_id = gts_id!("cf.core.events.topic.v1~"),
    description = "Topic resource. A named logical event stream. Sequencing, offsets, and ordering are scoped to a single (topic, partition), and the partition count is broker configuration rather than a property of the topic.",
    properties = "id,description,retention",
)]
pub struct TopicV1 {
    /// Full GTS topic identifier (e.g., gts.cf.core.events.topic.v1~vendor.users.v1).
    #[schemars(
        with = "String",
        extend("x-gts-instance" = gts_id!("cf.core.events.topic.v1~"))
    )]
    pub id: GtsInstanceId,
    /// What the stream carries, for a reader deciding whether to subscribe to it. ASCII only.
    #[schemars(regex(pattern = ASCII_PRINTABLE_OR_EMPTY), length(max = 1024))]
    pub description: String,
    /// ISO 8601 duration events on this topic are retained for. Absent means the broker's configured default.
    #[serde(default)]
    pub retention: Option<Iso8601Duration>,
}

/// Publish-time transport-metadata block. Stripped on read responses.
#[derive(Serialize, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(inline)]
pub struct EventMeta {
    /// Meta-block schema version. The broker accepts version <= current_supported and rejects newer with 400 UnknownMetaVersion.
    #[schemars(range(min = 1))]
    pub version: u32,
    /// Producer registered via POST /v1/producers; bound to the calling principal. Required for chained / monotonic mode publishes. Omit for stateless.
    #[serde(default)]
    pub producer_id: Option<Uuid>,
    /// Predecessor's sequence for chain dedup. Required and only valid in chained mode. On the first chained-mode publish for a (producer_id, topic, partition), set to 0.
    #[serde(default)]
    #[schemars(range(min = 0))]
    pub previous: Option<i64>,
    /// Producer-assigned monotonic sequence per (producer_id, topic, partition). Required in chained and monotonic modes; omitted in stateless.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub sequence: Option<i64>,
}

/// Base event type. Abstract: every concrete event type is a derived type schema
/// under it, narrowing `data` to its own payload contract and fixing its traits.
///
/// One canonical shape serves both publish input and read responses; direction is
/// encoded per field. `writeOnly` fields are accepted on publish and stripped on
/// read; `readOnly` fields are server-stamped on read and rejected if supplied on
/// publish. `required` is therefore the union of publish-required and
/// read-required, and a strict-validator producer filters `readOnly` fields
/// before submitting.
#[gts_type_schema(
    dir_path = "schemas",
    base = true,
    type_id = gts_id!("cf.core.events.event.v1~"),
    description = "Event schema. Single canonical resource shape used by both publish input (POST /v1/events, POST /v1/events:batch) and read responses (poll / query). Per-direction semantics are encoded via JSON Schema field-level markers: `writeOnly` fields (meta) are accepted on publish and stripped on read; `readOnly` fields (partition, sequence, sequence_time) are server-stamped on read and rejected with 400 BadRequest if supplied on publish. The top-level `required` list is the union of publish-required and read-required fields; strict-validator producers must filter `readOnly` fields before submission.",
    properties = "id,r#type,tenant_id,source,subject,subject_type,occurred_at,trace_parent,data,partition,sequence,sequence_time,meta",
    traits_schema = inline(EventTraits),
    gts_abstract = true,
)]
pub struct EventV1 {
    /// Client-provided unique event identifier (UUID).
    pub id: Uuid,
    /// Full GTS event-type identifier. The event is an instance of that derived type. ASCII only.
    // `with = "String"` so the field's own constraints land: a field typed
    // `GtsTypeId` is emitted as a `$ref`, and `gts-macros` then replaces the whole
    // property with what the type's `JsonSchema` impl says - an unregistered
    // `format` and a `gts.*` reference that every identifier matches.
    #[schemars(
        with = "String",
        regex(pattern = ASCII_PRINTABLE),
        length(max = 512),
        extend("x-gts-type" = gts_id!("cf.core.events.event.v1~"))
    )]
    pub r#type: GtsTypeId,
    /// Tenant the event belongs to. Producer-supplied. Ingest validates the producer's principal is authorized to publish to this tenant via the platform's authz resolver; unauthorized publishes are rejected with 403 TenantIdNotAuthorized.
    pub tenant_id: Uuid,
    /// Origin of the event (e.g., service name). ASCII only.
    #[schemars(regex(pattern = ASCII_PRINTABLE), length(max = 256))]
    pub source: String,
    /// Subject entity identifier for event correlation, filtering, and consumer semantics. An event type that needs subject-level ordering points its partition-key trait at this member. ASCII only.
    #[schemars(regex(pattern = ASCII_PRINTABLE), length(min = 1, max = 1024))]
    pub subject: String,
    /// Full GTS subject-type identifier - the type of the entity the event is about. Required and NOT derivable from `type`: event types may be generic (e.g., `rule_applied`) across multiple subject kinds, and events may have no `data` body to introspect. ASCII only.
    #[schemars(
        regex(pattern = ASCII_PRINTABLE),
        length(max = 512),
        extend("x-gts-type" = true)
    )]
    pub subject_type: String,
    /// When the event occurred (producer-stamped). ISO 8601 / RFC 3339 timestamp.
    #[schemars(extend("format" = "date-time"))]
    pub occurred_at: String,
    /// W3C Trace Context parent. Carries the trace context the event was produced under; surfaced on read responses for consumer-side trace correlation. Validated against the W3C traceparent header format.
    #[serde(default)]
    #[schemars(regex(pattern = r"^[0-9a-f]{2}-[0-9a-f]{32}-[0-9a-f]{16}-[0-9a-f]{2}$"))]
    pub trace_parent: Option<String>,
    /// Event payload, validated at ingest against the resolved schema of the event's type. The only field where UTF-8 (or any non-ASCII bytes) is permitted; all other event fields are ASCII per platform convention. May be absent for body-less events (e.g., notification-only events whose semantics are fully carried by `type` + `subject`).
    #[serde(default)]
    #[schemars(with = "Option<serde_json::Map<String, serde_json::Value>>")]
    pub data: Option<serde_json::Value>,
    /// Server-stamped partition for the event, surfaced on read. Computed at ingest via the partition contract. Producers MUST NOT supply this field on publish; doing so is rejected with 400 BadRequest.
    #[schemars(range(min = 0), extend("readOnly" = true))]
    pub partition: i32,
    /// Server-assigned monotonic ordering key per (topic, partition), surfaced on read. The only sequence consumers paginate by. Distinct from `meta.sequence` (publish-only producer-side chain field). Producers MUST NOT supply this field on publish; doing so is rejected with 400 BadRequest.
    #[schemars(range(min = 0), extend("readOnly" = true))]
    pub sequence: i64,
    /// Server-stamped timestamp recording when `sequence` was assigned. Surfaced on read. Producers MUST NOT supply this field on publish; doing so is rejected with 400 BadRequest.
    #[schemars(extend("readOnly" = true, "format" = "date-time"))]
    pub sequence_time: String,
    /// Optional publish-time transport-metadata block. Carries producer-protocol fields (producer_id, previous, sequence) for chained / monotonic modes. Omit entirely for stateless publish. Stripped on read responses; consumers MUST NOT rely on it.
    #[serde(default)]
    #[schemars(extend("writeOnly" = true))]
    pub meta: Option<EventMeta>,
}

/// Every `data` constraint a resolved event-type schema declares, composed into
/// one subschema.
///
/// A resolved event-type schema describes a whole event, and the base marks the
/// server-stamped members both `required` and `readOnly`, so validating a publish
/// payload against it directly would fail on fields the producer is forbidden to
/// send. Producer-side validation therefore targets the payload contract alone,
/// which is what each type in the chain narrows.
///
/// Resolution inlines each ancestor into its descendant's `allOf`, so a chain
/// deeper than two nests `allOf` within `allOf`. The walk recurses for that
/// reason: a flat pass over the immediate branches would silently drop the
/// constraints of every type above the parent. Branches are visited before a
/// node's own `properties.data`, so ancestors compose ahead of descendants and
/// the narrowest constraint lands last.
#[must_use]
pub fn data_contract(resolved: &serde_json::Value) -> serde_json::Value {
    /// Bounds the walk so a schema that resolved into a cycle cannot recurse
    /// forever. Real chains are two or three deep.
    const MAX_DEPTH: usize = 32;

    fn collect(node: &serde_json::Value, depth: usize, out: &mut Vec<serde_json::Value>) {
        if depth == MAX_DEPTH {
            return;
        }
        if let Some(branches) = node.get("allOf").and_then(serde_json::Value::as_array) {
            for branch in branches {
                collect(branch, depth + 1, out);
            }
        }
        if let Some(data) = node.get("properties").and_then(|props| props.get("data")) {
            out.push(data.clone());
        }
    }

    let mut branches = Vec::new();
    collect(resolved, 0, &mut branches);
    serde_json::json!({ "allOf": branches })
}

/// Assembles the derived event-type schema document for `type_id`: the payload
/// contract as a narrowing of the base's `data` member, and the governing
/// metadata as `x-gts-traits`.
///
/// Test fixtures and the mock register event types through this rather than
/// storing a bare payload schema, so what they hold is the same shape
/// `types-registry` provisions - which means the shape it will actually admit.
///
/// A `data_schema` with no constraints of its own is rendered as the base's own
/// `["object", "null"]` rather than passed through as an empty schema. An empty
/// schema admits any JSON value, which is *wider* than the base's member, and a
/// derived type may only narrow: registration refuses the document with
/// "Schema at '$.data' changes type incompatibly". A caller stating no
/// constraints means "whatever the base allows", and that is what this writes.
#[must_use]
pub fn derived_event_type_schema(
    type_id: &str,
    topic: &str,
    data_schema: serde_json::Value,
    allowed_subject_types: &[&str],
) -> serde_json::Value {
    serde_json::json!({
        "$id": format!("{}{type_id}", toolkit_gts::GTS_ID_URI_PREFIX),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "x-gts-traits": {
            "topic": topic,
            "allowed_subject_types": allowed_subject_types,
            "partition_key": TENANT_POINTER,
        },
        "type": "object",
        "allOf": [
            { "$ref": toolkit_gts::gts_uri!("cf.core.events.event.v1~") },
            {
                "type": "object",
                "properties": {
                    "data": if data_schema
                        .as_object()
                        .is_some_and(serde_json::Map::is_empty)
                    {
                        serde_json::json!({ "type": ["object", "null"] })
                    } else {
                        data_schema
                    },
                },
            },
        ],
    })
}

/// Consumer-group resource type - PEP/error matching only, no schema
/// generation (no `types-registry` inventory entry).
pub const CONSUMER_GROUP_RESOURCE_TYPE: &str = gts_id!("cf.core.events.consumer_group.v1~");
/// Event-type resource type - PEP/error matching only. The GTS type it names
/// no longer exists as a registered type (an event type is a derived schema of
/// `EventV1` now), but authorization and error payloads still identify the
/// event-type resource by this id, and like its siblings here it has no
/// `types-registry` inventory entry to disagree with.
pub const EVENT_TYPE_RESOURCE_TYPE: &str = gts_id!("cf.core.events.event_type.v1~");
/// Subscription resource type - PEP/error matching only.
pub const SUBSCRIPTION_RESOURCE_TYPE: &str = gts_id!("cf.core.events.subscription.v1~");
/// Producer resource type - PEP/error matching only.
pub const PRODUCER_RESOURCE_TYPE: &str = gts_id!("cf.core.events.producer.v1~");
/// Generic fallback resource type for errors not tied to one entity kind
/// (mirrors `api/rest/error.rs::EventBrokerResourceError`'s own fallback).
pub const REQUEST_RESOURCE_TYPE: &str = gts_id!("cf.core.events.request.v1~");
