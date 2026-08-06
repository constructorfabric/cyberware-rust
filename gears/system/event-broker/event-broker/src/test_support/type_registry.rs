//! Declarative topic/event-type fixtures for `EventBrokerHarness`.
//!
//! Every entry becomes the document `types-registry` would actually hold - a
//! topic as an instance of the topic base type, an event type as a derived type
//! schema of the abstract event base type - and the harness loads them through
//! the same path a booting process uses. Building the broker's own structs
//! directly, as this used to, is what let a whole unit suite pass against a
//! process that could not load a topic at all.
//!
//! `StaticTypesRegistry::of(...)` takes a flat list; each entry's `id` alone
//! says which kind it is, since there is exactly one base type per kind, and
//! every id is validated as the GTS kind it claims to be.

use std::collections::HashSet;
use std::sync::Arc;

use event_broker_sdk::gts::{EventV1, TopicV1};
use gts::GtsTypeId;
use serde_json::{Value as JsonValue, json};
use toolkit_gts::{GtsInstanceId, GtsSchema};
use types_registry_sdk::GtsTypeSchema;

use crate::config::EventBrokerConfig;

const TOPIC_BASE: &str = TopicV1::TYPE_ID;
const EVENT_BASE: &str = EventV1::TYPE_ID;

/// The backend every fixture deployment names. A fixture never opens it - the
/// harness builds the `SQLite` backend directly - but configuration has to name
/// a backend type for the same reason production does.
const SQLITE_BACKEND: &str = "gts.cf.core.events.backend.v1~cf.core.backend.sqlite.v1~";

/// A stand-in for the registered base event type.
///
/// Carries what resolution actually reads: the members a partition-key pointer
/// may name, and the trait schema whose default reaches a type that declares no
/// pointer of its own. The real base is emitted from `event-broker-sdk`'s
/// declaration into the process-wide GTS inventory; pulling that inventory into
/// a unit test would be testing the macro rather than the broker.
#[must_use]
pub fn event_base_schema() -> Arc<GtsTypeSchema> {
    Arc::new(
        GtsTypeSchema::try_new(
            GtsTypeId::new(EVENT_BASE),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "tenant_id": { "type": "string" },
                    "source": { "type": "string" },
                    "subject": { "type": "string" },
                    "subject_type": { "type": "string" },
                    "occurred_at": { "type": "string" },
                    "data": { "type": ["object", "null"] },
                },
                "x-gts-traits-schema": {
                    "properties": { "partition_key": { "default": "/tenant_id" } },
                },
            }),
            None,
            None,
        )
        .expect("the base event type schema is valid"),
    )
}

/// A derived event-type schema over [`event_base_schema`], its body built by
/// the same helper the mock and the production fixtures use, so what a test
/// registers is the shape `types-registry` provisions.
///
/// # Panics
/// Panics if `type_id` is not a well-formed GTS type identifier.
#[must_use]
pub fn derived_event_type(
    type_id: &str,
    topic: &str,
    data_schema: JsonValue,
    allowed_subject_types: &[&str],
) -> GtsTypeSchema {
    schema_from_body(
        type_id,
        event_broker_sdk::gts::derived_event_type_schema(
            type_id,
            topic,
            data_schema,
            allowed_subject_types,
        ),
    )
}

/// A derived schema over [`event_base_schema`] from a body the caller has
/// already shaped - which is how a fixture overrides a trait the standard
/// builder fixes, such as pointing the partition key at a payload member.
///
/// # Panics
/// Panics if `type_id` is not a well-formed GTS type identifier.
#[must_use]
pub fn schema_from_body(type_id: &str, body: JsonValue) -> GtsTypeSchema {
    GtsTypeSchema::try_new(
        GtsTypeId::try_new(type_id)
            .unwrap_or_else(|err| panic!("'{type_id}' is not a valid GTS type id: {err}")),
        body,
        None,
        Some(event_base_schema()),
    )
    .expect("a derived event-type schema is valid")
}

/// Declarative fixtures, applied by `EventBrokerHarnessBuilder::build()`.
pub struct StaticTypesRegistry {
    /// Topic instance documents, exactly as an owning gear would register them.
    pub(super) topics: Vec<JsonValue>,
    /// Derived event-type schemas, with the base as their resolved parent.
    pub(super) event_types: Vec<GtsTypeSchema>,
    /// The configuration the partition counts came from: a topic carries no
    /// count of its own, so a fixture that asks for four partitions is asking
    /// for a deployment configured that way.
    pub(super) config: EventBrokerConfig,
}

impl StaticTypesRegistry {
    /// `spec` is a flat JSON array; each entry's `id` prefix decides what
    /// fields it needs:
    /// - a topic (`{TOPIC_BASE}...`, no trailing `~`): `partitions` (integer,
    ///   required) - written into configuration rather than onto the topic,
    ///   because that is where a partition count lives.
    /// - an event type (`{EVENT_BASE}...~`, trailing `~` required, since a
    ///   concrete event type is a derived type schema): `topic` (the `id` of a
    ///   topic declared earlier in the same list, required), `data_schema`
    ///   (object, defaults to `{}`), `allowed_subject_types` (array of GTS
    ///   Type-id pattern strings, defaults to `[]` - the true minimal value,
    ///   which rejects every `subject_type` at publish time), and
    ///   `partition_key` (a JSON Pointer, defaulting to the tenant the base
    ///   declares).
    ///
    /// An event type may name a topic deliberately left unregistered - the only
    /// way to reach the broker's "the type resolves a topic nobody registered"
    /// branch - with `"topic_registered": false`.
    ///
    /// # Panics
    /// Panics (fixture-construction failure, not a runtime `Result`) if `spec`
    /// is not a JSON array; an entry is missing `id` or a kind-required field;
    /// an `id` is not well-formed for its kind; an `id`'s base type is neither
    /// the topic nor the event base; or an event type's `topic` was not declared
    /// earlier in the same list.
    #[must_use]
    #[allow(
        clippy::needless_pass_by_value,
        reason = "taking JsonValue by value lets every call site pass a bare `json!(...)` literal"
    )]
    pub fn of(spec: JsonValue) -> Self {
        let entries = spec
            .as_array()
            .expect("StaticTypesRegistry::of expects a JSON array");
        let mut topics = Vec::new();
        let mut event_types = Vec::new();
        let mut topic_ids: HashSet<String> = HashSet::new();
        let mut settings = serde_json::Map::new();

        for entry in entries {
            let id = entry["id"].as_str().unwrap_or_else(|| {
                panic!("StaticTypesRegistry entry missing a string 'id': {entry}")
            });

            if id.starts_with(TOPIC_BASE) && !id.ends_with('~') {
                GtsInstanceId::try_new(id)
                    .unwrap_or_else(|err| panic!("'{id}' is not a valid GTS instance id: {err}"));
                let partitions = entry["partitions"].as_i64().unwrap_or_else(|| {
                    panic!("topic '{id}' is missing an integer 'partitions' field")
                });
                topic_ids.insert(id.to_owned());
                topics.push(json!({
                    "id": id,
                    "description": "a topic this test publishes to",
                }));
                settings.insert(id.to_owned(), json!({ "partitions": partitions }));
            } else if id.starts_with(EVENT_BASE) {
                let type_id = GtsTypeId::try_new(id).unwrap_or_else(|err| {
                    panic!(
                        "'{id}' is not a valid GTS type id: {err} - a concrete event type is a \
                         derived type schema, so its identifier ends in '~'"
                    )
                });
                let topic = entry["topic"]
                    .as_str()
                    .unwrap_or_else(|| panic!("event type '{id}' is missing a string 'topic'"));
                let topic_registered = entry
                    .get("topic_registered")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(true);
                assert!(
                    !topic_registered || topic_ids.contains(topic),
                    "event type '{id}' references topic '{topic}', which isn't declared earlier \
                     in this StaticTypesRegistry::of(...) list - add \"topic_registered\": false \
                     if that is the point of the fixture"
                );
                let allowed: Vec<&str> = entry
                    .get("allowed_subject_types")
                    .map(|value| {
                        value
                            .as_array()
                            .unwrap_or_else(|| {
                                panic!("event type '{id}' 'allowed_subject_types' must be an array")
                            })
                            .iter()
                            .map(|entry| {
                                entry.as_str().unwrap_or_else(|| {
                                    panic!(
                                        "event type '{id}' 'allowed_subject_types' entries must \
                                         be strings"
                                    )
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let data_schema = entry.get("data_schema").cloned().unwrap_or(json!({}));
                let mut body = event_broker_sdk::gts::derived_event_type_schema(
                    id,
                    topic,
                    data_schema,
                    &allowed,
                );
                // The standard builder fixes the pointer at the tenant. An
                // entry may override it, which is how a fixture exercises a
                // type that groups by a payload member.
                if let Some(pointer) = entry.get("partition_key").and_then(JsonValue::as_str) {
                    body["x-gts-traits"]["partition_key"] = json!(pointer);
                }
                let _ = type_id;
                event_types.push(schema_from_body(id, body));
            } else {
                panic!(
                    "'{id}' is neither a topic instance ('{TOPIC_BASE}...') nor an event type \
                     ('{EVENT_BASE}...~') - StaticTypesRegistry seeds these two kinds only"
                );
            }
        }

        Self {
            topics,
            event_types,
            config: serde_json::from_value(json!({
                "mode": "standalone",
                "default_storage_backend": SQLITE_BACKEND,
                "topics": settings,
            }))
            .expect("fixture configuration deserializes"),
        }
    }

    /// The configuration a fixture-free harness runs with: no topics named, so
    /// every topic resolves to the built-in tier.
    #[must_use]
    pub fn empty_config() -> EventBrokerConfig {
        serde_json::from_value(json!({
            "mode": "standalone",
            "default_storage_backend": SQLITE_BACKEND,
            "topics": {},
        }))
        .expect("fixture configuration deserializes")
    }
}
