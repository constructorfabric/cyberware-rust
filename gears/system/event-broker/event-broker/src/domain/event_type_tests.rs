//! Unit tests for the topic resolution in `event_type.rs`.
//!
//! Kept in a sibling `_tests.rs` file per the repo's tests-in-separate-files
//! lint, linked in from `event_type.rs`.

use std::sync::Arc;

use gts::{GtsInstanceId, GtsTypeId};
use serde_json::json;
use types_registry_sdk::GtsTypeSchema;

use super::{topic, validate_partition_key};
use crate::domain::error::DomainError;

/// The base type the broker owns; abstract, and the only declarer of the
/// event trait schema.
const BASE_ID: &str = "gts.cf.core.events.event.v1~";
/// The committed worked example under `docs/schemas/examples/`.
const DERIVED_ID: &str = "gts.cf.core.events.event.v1~fabrikam.shop.orders.order_placed.v1~";
/// A topic is an instance, so the example topic's identifier does not end in `~`.
const EXAMPLE_TOPIC: &str = "gts.cf.core.events.topic.v1~fabrikam.shop._.orders.v1";

/// The base's `x-gts-traits-schema` as the broker declares it - `topic`
/// required, `allowed_subject_types` defaulting to `[]`, `partition_key`
/// defaulting to the tenant member the base also declares. Trait defaults are
/// resolved from this block, so the chain must carry it for resolution to
/// behave as it does in production.
fn base() -> Arc<GtsTypeSchema> {
    Arc::new(
        GtsTypeSchema::try_new(
            GtsTypeId::new(BASE_ID),
            json!({
                "type": "object",
                "x-gts-abstract": true,
                "properties": {
                    "tenant_id": { "type": "string" },
                    "data": { "type": ["object", "null"] }
                },
                "x-gts-traits-schema": {
                    "type": "object",
                    "required": ["topic"],
                    "properties": {
                        "topic": { "type": "string" },
                        "allowed_subject_types": {
                            "type": "array",
                            "default": [],
                            "items": { "type": "string" }
                        },
                        "partition_key": {
                            "type": "string",
                            "format": "json-pointer",
                            "default": "/tenant_id"
                        }
                    }
                }
            }),
            None,
            None,
        )
        .unwrap(),
    )
}

fn derived(traits: &serde_json::Value) -> GtsTypeSchema {
    GtsTypeSchema::try_new(
        GtsTypeId::new(DERIVED_ID),
        json!({
            "$id": format!("gts://{DERIVED_ID}"),
            "type": "object",
            "x-gts-traits": traits,
            "allOf": [
                { "$ref": format!("gts://{BASE_ID}") },
                {
                    "type": "object",
                    "properties": {
                        "data": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["order_id"],
                            "properties": {
                                "order_id": { "type": "string", "format": "uuid" }
                            }
                        }
                    }
                }
            ]
        }),
        None,
        Some(base()),
    )
    .unwrap()
}

#[test]
fn topic_resolves_the_declared_trait_as_a_gts_type_id() {
    let event_type = derived(&json!({
        "topic": EXAMPLE_TOPIC,
        "allowed_subject_types": ["gts.fabrikam.shop.orders.order.v1~"],
    }));

    let resolved = topic(&event_type).expect("the declared topic trait must resolve");

    assert_eq!(resolved, GtsInstanceId::try_new(EXAMPLE_TOPIC).unwrap());
}

#[test]
fn topic_inherits_a_value_declared_further_up_the_chain() {
    // A base may set a topic that derived types publish to without restating
    // it; `effective_traits` merges along the chain, so resolution must too.
    let inherited_base = Arc::new(
        GtsTypeSchema::try_new(
            GtsTypeId::new(BASE_ID),
            json!({ "type": "object", "x-gts-traits": { "topic": EXAMPLE_TOPIC } }),
            None,
            None,
        )
        .unwrap(),
    );
    let event_type = GtsTypeSchema::try_new(
        GtsTypeId::new(DERIVED_ID),
        json!({ "type": "object", "allOf": [{ "$ref": format!("gts://{BASE_ID}") }] }),
        None,
        Some(inherited_base),
    )
    .unwrap();

    let resolved = topic(&event_type).expect("an inherited topic trait must resolve");

    assert_eq!(resolved, GtsInstanceId::try_new(EXAMPLE_TOPIC).unwrap());
}

#[test]
fn topic_is_an_internal_error_when_no_trait_is_declared_anywhere_in_the_chain() {
    let event_type = derived(&json!({ "allowed_subject_types": [] }));

    let err = topic(&event_type)
        .expect_err("an event type with no topic trait names no stream and cannot be published to");

    assert!(matches!(err, DomainError::Internal(_)));
    assert_eq!(
        err.to_string(),
        "internal error: gts.cf.core.events.event.v1~fabrikam.shop.orders.order_placed.v1~ \
         resolves to no `topic` trait value"
    );
}

#[test]
fn topic_is_an_internal_error_when_the_trait_names_a_type_rather_than_an_instance() {
    // A trailing `~` makes it a type id, and a topic is always an instance: it
    // carries the stream's own data, and nothing derives from it.
    let event_type =
        derived(&json!({ "topic": "gts.cf.core.events.topic.v1~fabrikam.shop._.orders.v1~" }));

    let err = topic(&event_type).expect_err("a topic trait must name a GTS instance");

    assert_eq!(
        err.to_string(),
        "internal error: gts.cf.core.events.event.v1~fabrikam.shop.orders.order_placed.v1~ \
         names `gts.cf.core.events.topic.v1~fabrikam.shop._.orders.v1~` as its topic, \
         which is not a GTS instance id: \
         Invalid GTS identifier: gts.cf.core.events.topic.v1~fabrikam.shop._.orders.v1~: \
         GTS instance IDs must not end with '~' (a trailing '~' denotes a type id)"
    );
}

// -- validate_partition_key ----------------------------------------------------

/// The base's own `data` member, so a pointer into the payload has a parent to
/// descend through.
fn derived_with_payload(traits: &serde_json::Value) -> GtsTypeSchema {
    GtsTypeSchema::try_new(
        GtsTypeId::new(DERIVED_ID),
        json!({
            "$id": format!("gts://{DERIVED_ID}"),
            "type": "object",
            "x-gts-traits": traits,
            "allOf": [
                { "$ref": format!("gts://{BASE_ID}") },
                {
                    "type": "object",
                    "properties": {
                        "data": {
                            "type": "object",
                            "properties": { "order_id": { "type": "string" } },
                        },
                    },
                },
            ],
        }),
        None,
        Some(base()),
    )
    .expect("derived schema fixture")
}

#[test]
fn a_pointer_into_the_payload_is_accepted() {
    let event_type = derived_with_payload(&json!({
        "topic": EXAMPLE_TOPIC,
        "partition_key": "/data/order_id",
    }));

    validate_partition_key(&event_type).expect("a pointer naming a declared payload member holds");
}

#[test]
fn a_pointer_naming_a_base_member_is_accepted() {
    // The default. It names nothing the derived type declares itself, so this is
    // the case that proves inherited members count.
    let event_type = derived_with_payload(&json!({ "topic": EXAMPLE_TOPIC }));

    validate_partition_key(&event_type).expect("a pointer naming an inherited member holds");
}

#[test]
fn a_pointer_naming_nothing_is_rejected_and_names_itself() {
    let event_type = derived_with_payload(&json!({
        "topic": EXAMPLE_TOPIC,
        "partition_key": "/data/no_such_member",
    }));

    let err = validate_partition_key(&event_type)
        .expect_err("a pointer naming an undeclared member cannot be admitted");

    assert_eq!(
        err.to_string(),
        "InvalidSpec: gts.cf.core.events.event.v1~fabrikam.shop.orders.order_placed.v1~ \
         declares partition key `/data/no_such_member`, which names no declared member \
         `no_such_member`"
    );
}

#[test]
fn a_value_that_is_not_a_pointer_is_rejected() {
    let event_type = derived_with_payload(&json!({
        "topic": EXAMPLE_TOPIC,
        "partition_key": "tenant_id",
    }));

    let err = validate_partition_key(&event_type).expect_err("a bare field name is not a pointer");

    assert_eq!(
        err.to_string(),
        "InvalidSpec: gts.cf.core.events.event.v1~fabrikam.shop.orders.order_placed.v1~ \
         declares partition key `tenant_id`, which is not a JSON Pointer into the event"
    );
}
