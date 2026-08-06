//! Tests for [`crate::domain::projection`], the single place a resolved GTS type
//! schema becomes one of the broker's API shapes.
//!
//! Fixtures are built the way production builds them: the base is the schema the
//! broker's own declaration emits, so its `x-gts-traits-schema` - and therefore
//! trait defaulting - is exercised rather than bypassed, and every derived
//! schema carries its resolved parent, which `GtsTypeSchema::try_new` requires.

use std::sync::Arc;

use event_broker_sdk::gts::{EventV1, TopicV1};
use serde_json::json;
use types_registry_sdk::{GtsInstance, GtsTypeSchema};

use crate::api::rest::handlers::ingest::dto::EventTypeDto;
use crate::domain::error::DomainError;
use crate::domain::projection;

const TOPIC_BASE: &str = "gts.cf.core.events.topic.v1~";
const EVENT_BASE: &str = "gts.cf.core.events.event.v1~";

const AUDIT_TOPIC: &str = "gts.cf.core.events.topic.v1~example.proj.broker.audit.v1";
const NOTIFY_TOPIC: &str = "gts.cf.core.events.topic.v1~example.proj.broker.notify.v1";

const ORDER_TYPE: &str = "gts.cf.core.events.event.v1~example.proj.shop.order.v1~";
const ORDER_EU_TYPE: &str =
    "gts.cf.core.events.event.v1~example.proj.shop.order.v1~example.proj.shop.order_eu.v1~";

/// `description` of the base event's `data` member. It is the ancestor branch of
/// every composed payload contract, spelled out here so that changing the base
/// surfaces as a failure in these tests.
const BASE_DATA_DESCRIPTION: &str = "Event payload, validated at ingest against the resolved schema of the event's type. The only field where UTF-8 (or any non-ASCII bytes) is permitted; all other event fields are ASCII per platform convention. May be absent for body-less events (e.g., notification-only events whose semantics are fully carried by `type` + `subject`).";

/// The base event's `data` member as `data_contract` composes it into the first
/// branch of a payload contract.
fn base_data_member() -> serde_json::Value {
    json!({
        "additionalProperties": true,
        "default": null,
        "description": BASE_DATA_DESCRIPTION,
        "type": ["object", "null"],
    })
}

// -- Fixture construction ------------------------------------------------------

fn topic_base() -> Arc<GtsTypeSchema> {
    base(TOPIC_BASE, &TopicV1::gts_schema_with_refs_as_string())
}

fn event_base() -> Arc<GtsTypeSchema> {
    base(EVENT_BASE, &EventV1::gts_schema_with_refs_as_string())
}

fn base(type_id: &str, emitted: &str) -> Arc<GtsTypeSchema> {
    let raw_schema = serde_json::from_str(emitted).expect("emitted base schema is valid JSON");
    Arc::new(
        GtsTypeSchema::try_new(gts::GtsTypeId::new(type_id), raw_schema, None, None)
            .expect("base schema fixture"),
    )
}

fn derived(
    type_id: &str,
    raw_schema: serde_json::Value,
    parent: &Arc<GtsTypeSchema>,
) -> GtsTypeSchema {
    GtsTypeSchema::try_new(
        gts::GtsTypeId::new(type_id),
        raw_schema,
        None,
        Some(Arc::clone(parent)),
    )
    .expect("derived schema fixture")
}

/// A derived schema body: the trait values it fixes, plus a `$ref` to the type
/// it chains under. The shape both `types-registry` and the mock provision.
fn body(type_id: &str, parent_type_id: &str, traits: &serde_json::Value) -> serde_json::Value {
    json!({
        "$id": format!("gts://{type_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "x-gts-traits": traits,
        "type": "object",
        "allOf": [{ "$ref": format!("gts://{parent_type_id}") }],
    })
}

/// A registered topic: the instance document, chained under the topic base whose
/// properties it must satisfy. Built the way production builds it, so the base's
/// own declaration is what the instance is held to.
fn topic_instance(document: serde_json::Value) -> GtsInstance {
    let id = document["id"]
        .as_str()
        .expect("fixture document carries an id");
    GtsInstance::try_new(
        gts::GtsInstanceId::try_new(id).expect("fixture topic id is an instance id"),
        document,
        None,
        topic_base(),
    )
    .expect("topic instance fixture")
}

fn internal_detail(err: DomainError) -> String {
    match err {
        DomainError::Internal(detail) => detail,
        other => panic!("expected Internal, got {other:?}"),
    }
}

// -- topic ---------------------------------------------------------------------

#[test]
fn topic_projects_a_document_that_declares_no_retention() {
    // `retention` is optional on the base, so a topic that declares none projects
    // with it absent - the broker's configured default applies instead.
    let instance = topic_instance(json!({
        "id": AUDIT_TOPIC,
        "description": "Audit stream.",
    }));

    let topic = projection::topic(&instance).unwrap();
    assert_eq!(topic.id.as_ref(), AUDIT_TOPIC);
    assert_eq!(topic.description, "Audit stream.");
    assert_eq!(topic.retention, None);
}

#[test]
fn topic_projects_a_declared_retention_in_canonical_units() {
    // Authored with a day component on purpose: the projection parses the value
    // into a duration, so the reported form below is canonical hours.
    let instance = topic_instance(json!({
        "id": NOTIFY_TOPIC,
        "description": "Notification stream.",
        "retention": "P7D",
    }));

    let topic = projection::topic(&instance).unwrap();
    assert_eq!(topic.id.as_ref(), NOTIFY_TOPIC);
    assert_eq!(topic.description, "Notification stream.");
    assert_eq!(
        topic.retention.map(|r| r.to_string()).as_deref(),
        Some("PT168H")
    );
}

#[test]
fn topic_without_a_description_is_internal() {
    // The base requires `description`, so a document without one is a
    // registration `types-registry` should not have admitted.
    let instance = topic_instance(json!({ "id": AUDIT_TOPIC }));

    assert_eq!(
        internal_detail(projection::topic(&instance).unwrap_err()),
        "gts.cf.core.events.topic.v1~example.proj.broker.audit.v1 carries no string \
         `description`"
    );
}

#[test]
fn topic_with_an_unparseable_retention_is_internal() {
    let instance = topic_instance(json!({
        "id": AUDIT_TOPIC,
        "description": "Audit stream.",
        "retention": "7 days",
    }));

    assert!(
        internal_detail(projection::topic(&instance).unwrap_err()).starts_with(
            "gts.cf.core.events.topic.v1~example.proj.broker.audit.v1 has a `retention` that is \
             not an ISO 8601 duration"
        )
    );
}

// A topic no longer resolves anything along an inheritance chain: it is an
// instance, and an instance cannot derive from an instance. Trait inheritance
// itself stays covered by `event_type_inherits_traits_from_an_ancestor`, which
// exercises the same chain-merge on the shape that still has one.

// -- event_type ----------------------------------------------------------------

#[test]
fn event_type_defaults_allowed_subject_types_to_empty() {
    // No level of the chain declares the trait, so the base's default stands.
    let schema = derived(
        ORDER_TYPE,
        body(ORDER_TYPE, EVENT_BASE, &json!({ "topic": AUDIT_TOPIC })),
        &event_base(),
    );

    assert_eq!(
        serde_json::to_value(EventTypeDto::from(projection::event_type(&schema).unwrap())).unwrap(),
        json!({
            "id": ORDER_TYPE,
            "topic": AUDIT_TOPIC,
            "partition_key": "/tenant_id",
            "description": null,
            "allowed_subject_types": [],
            // The type narrows nothing, so the base's `data` member is the whole
            // payload contract.
            "data_schema": { "allOf": [base_data_member()] },
        })
    );
}

#[test]
fn event_type_inherits_traits_from_an_ancestor() {
    // The leaf re-anchors the type to another topic but says nothing about
    // subject types, so the parent's list resolves.
    let parent = Arc::new(derived(
        ORDER_TYPE,
        body(
            ORDER_TYPE,
            EVENT_BASE,
            &json!({
                "topic": AUDIT_TOPIC,
                "allowed_subject_types": ["gts.cf.core.events.subject.v1~example.proj.shop.order.v1~"],
            }),
        ),
        &event_base(),
    ));
    let schema = derived(
        ORDER_EU_TYPE,
        body(ORDER_EU_TYPE, ORDER_TYPE, &json!({ "topic": NOTIFY_TOPIC })),
        &parent,
    );

    assert_eq!(
        serde_json::to_value(EventTypeDto::from(projection::event_type(&schema).unwrap())).unwrap(),
        json!({
            "id": ORDER_EU_TYPE,
            "topic": NOTIFY_TOPIC,
            "partition_key": "/tenant_id",
            "description": null,
            "allowed_subject_types": [
                "gts.cf.core.events.subject.v1~example.proj.shop.order.v1~",
            ],
            // Neither level narrows `data`, so the base's own member is the whole
            // payload contract - reached through two levels of resolved nesting.
            "data_schema": { "allOf": [base_data_member()] },
        })
    );
}

#[test]
fn event_type_composes_the_data_narrowings_with_the_leaf_last() {
    let schema = derived(
        ORDER_TYPE,
        json!({
            "$id": format!("gts://{ORDER_TYPE}"),
            "$schema": "http://json-schema.org/draft-07/schema#",
            "x-gts-traits": { "topic": AUDIT_TOPIC },
            "type": "object",
            "allOf": [
                { "$ref": format!("gts://{EVENT_BASE}") },
                {
                    "type": "object",
                    "properties": {
                        "data": { "type": "object", "required": ["order_id"] },
                    },
                },
            ],
        }),
        &event_base(),
    );

    assert_eq!(
        serde_json::to_value(EventTypeDto::from(projection::event_type(&schema).unwrap())).unwrap(),
        json!({
            "id": ORDER_TYPE,
            "topic": AUDIT_TOPIC,
            "partition_key": "/tenant_id",
            "description": null,
            "allowed_subject_types": [],
            "data_schema": {
                "allOf": [
                    base_data_member(),
                    { "type": "object", "required": ["order_id"] },
                ],
            },
        })
    );
}

#[test]
fn event_type_without_a_topic_trait_is_internal() {
    let schema = derived(
        ORDER_TYPE,
        body(
            ORDER_TYPE,
            EVENT_BASE,
            &json!({ "allowed_subject_types": [] }),
        ),
        &event_base(),
    );

    assert_eq!(
        internal_detail(projection::event_type(&schema).unwrap_err()),
        "gts.cf.core.events.event.v1~example.proj.shop.order.v1~ resolves to no `topic` trait value"
    );
}

#[test]
fn event_type_naming_a_type_shaped_topic_is_internal() {
    // A topic is an instance, so an id with the trailing `~` names a type and
    // cannot be the stream an event type publishes to.
    let type_shaped = "gts.cf.core.events.topic.v1~example.proj.broker.audit.v1~";
    let schema = derived(
        ORDER_TYPE,
        body(ORDER_TYPE, EVENT_BASE, &json!({ "topic": type_shaped })),
        &event_base(),
    );

    assert_eq!(
        internal_detail(projection::event_type(&schema).unwrap_err()),
        "gts.cf.core.events.event.v1~example.proj.shop.order.v1~ names \
         `gts.cf.core.events.topic.v1~example.proj.broker.audit.v1~` as its topic, which is not a \
         GTS instance id: Invalid GTS identifier: \
         gts.cf.core.events.topic.v1~example.proj.broker.audit.v1~: \
         GTS instance IDs must not end with '~' (a trailing '~' denotes a type id)"
    );
}
