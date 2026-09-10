//! Unit tests for the license resolver GTS types.
//!
//! Beyond type ids, the abstract marker, and traits, these are **drift guards**
//! on the generated schema's actual payload contract: they assert the wire
//! shape (contract type + optional instance id + metadata object), so a
//! `gts-macros` schema regression fails loudly.

use gts::GtsSchema;
use serde_json::{Value, json};

use super::{LicenseResolverPluginSpecV1, LicenseResourceV1, LicenseSubjectV1};

// Derived contract types, as a consuming Gear registers them.

#[toolkit_gts::gts_type_schema(
    dir_path = "schemas",
    base = LicenseResourceV1,
    type_id = toolkit_gts::gts_id!("cf.core.lic.res.v1~cf.genai.llm_gateway.model_usage.v1~"),
    description = "test derived Resource contract",
    properties = "model_vendor,model_name"
)]
#[derive(Default)]
struct ModelUsageResourceV1 {
    model_vendor: String,
    model_name: String,
}

#[toolkit_gts::gts_type_schema(
    dir_path = "schemas",
    base = LicenseSubjectV1,
    type_id = toolkit_gts::gts_id!("cf.core.lic.subj.v1~cf.genai.llm_gateway.user.v1~"),
    description = "test derived Subject contract",
    properties = "category"
)]
#[derive(Default)]
struct UserSubjectV1 {
    category: String,
}

fn subject_schema() -> Value {
    serde_json::from_str(&LicenseSubjectV1::<()>::gts_schema_with_refs_as_string())
        .expect("subject schema must be valid JSON")
}

fn resource_schema() -> Value {
    serde_json::from_str(&LicenseResourceV1::<()>::gts_schema_with_refs_as_string())
        .expect("resource schema must be valid JSON")
}

/// The `metadata` sub-schema a derived contract refines (the second `allOf`
/// branch of the derived schema).
fn derived_metadata_subschema(derived_schema: &Value) -> Value {
    derived_schema
        .pointer("/allOf/1/properties/metadata")
        .cloned()
        .unwrap_or_else(|| panic!("derived schema must refine metadata:\n{derived_schema:#}"))
}

fn model_usage_schema() -> Value {
    serde_json::from_str(&ModelUsageResourceV1::gts_schema_with_refs_as_string()).unwrap()
}

fn user_subject_schema() -> Value {
    serde_json::from_str(&UserSubjectV1::gts_schema_with_refs_as_string()).unwrap()
}

#[test]
fn base_type_ids_are_canonical() {
    assert_eq!(
        LicenseSubjectV1::<()>::TYPE_ID,
        toolkit_gts::gts_id!("cf.core.lic.subj.v1~")
    );
    assert_eq!(
        LicenseResourceV1::<()>::TYPE_ID,
        toolkit_gts::gts_id!("cf.core.lic.res.v1~")
    );
}

#[test]
fn plugin_spec_derives_from_toolkit_plugin_base() {
    assert_eq!(
        LicenseResolverPluginSpecV1::TYPE_ID,
        toolkit_gts::gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.license_resolver.plugin.v1~")
    );
}

/// Exercises the inventory `schema_fn` closures the macro registers — the
/// registry path a Gear host reads, distinct from the direct accessors the
/// other tests call.
#[test]
fn inventory_registers_a_valid_schema_for_every_sdk_type() {
    let schemas = toolkit_gts::all_inventory_type_schemas()
        .expect("every registered schema_fn must render valid JSON");
    let ids: Vec<&str> = toolkit_gts::inventory::iter::<toolkit_gts::InventoryTypeSchema>
        .into_iter()
        .map(|e| e.type_id)
        .collect();
    for type_id in [
        LicenseSubjectV1::<()>::TYPE_ID,
        LicenseResourceV1::<()>::TYPE_ID,
        LicenseResolverPluginSpecV1::TYPE_ID,
    ] {
        assert!(
            ids.contains(&type_id),
            "{type_id} not registered; got ids: {ids:?}"
        );
    }
    assert_eq!(
        schemas.len(),
        ids.len(),
        "every registered entry must render a schema"
    );
}

/// The GTS store rejects an `x-gts-traits-schema` with no values in the
/// derivation chain, so the abstract base must declare the trait defaults.
#[test]
fn resource_base_declares_empty_admitted_subjects_trait_default() {
    assert_eq!(
        LicenseResourceV1::<()>::gts_traits(),
        Some(json!({ "admitted_subjects": [] }))
    );
}

#[test]
fn base_types_are_abstract() {
    for schema in [subject_schema(), resource_schema()] {
        assert_eq!(
            schema.get("x-gts-abstract"),
            Some(&Value::Bool(true)),
            "base licensing types must be abstract:\n{schema:#}"
        );
    }
}

#[test]
fn resource_base_emits_admitted_subjects_trait_referencing_subject_contract() {
    let schema = resource_schema();
    let x_gts_ref =
        schema.pointer("/x-gts-traits-schema/properties/admitted_subjects/items/x-gts-ref");
    assert_eq!(
        x_gts_ref.and_then(Value::as_str),
        Some(toolkit_gts::gts_id!("cf.core.lic.subj.v1~")),
        "admitted_subjects items must x-gts-ref the Subject licensing base:\n{schema:#}"
    );
}

#[test]
fn base_resource_requires_type_and_allows_optional_id() {
    let schema = resource_schema();
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();

    assert!(
        required.contains(&"type"),
        "`type` (contract type) must be required"
    );

    assert!(
        !required.contains(&"id"),
        "`id` must be optional (whole-type checks omit it)"
    );
    assert_eq!(
        schema.pointer("/properties/id/type"),
        Some(&json!(["string", "null"])),
        "`id` is an optional natural-key string:\n{schema:#}"
    );
    assert_eq!(
        schema.pointer("/properties/id/minLength"),
        Some(&json!(1)),
        "an empty-string id must be non-conforming:\n{schema:#}"
    );
    assert_eq!(
        schema.pointer("/properties/id/maxLength"),
        Some(&json!(255)),
        "id must carry the base contract's length bound:\n{schema:#}"
    );

    assert_eq!(
        schema.pointer("/properties/metadata/type"),
        Some(&json!("object"))
    );

    // `type` is narrowed to this base — `GtsTypeId`'s own schema would leave it
    // open (`gts.*`), admitting a Subject contract id in the resource slot.
    assert_eq!(
        schema.pointer("/properties/type/x-gts-ref"),
        Some(&json!(toolkit_gts::gts_id!("cf.core.lic.res.v1~"))),
        "resource `type` must ref the Resource base:\n{schema:#}"
    );
}

#[test]
fn base_subject_requires_type_and_allows_optional_id() {
    let schema = subject_schema();
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(required.contains(&"type"));
    assert!(!required.contains(&"id"));
    assert_eq!(
        schema.pointer("/properties/id/type"),
        Some(&json!(["string", "null"]))
    );
    assert_eq!(
        schema.pointer("/properties/id/minLength"),
        Some(&json!(1)),
        "an empty-string id must be non-conforming:\n{schema:#}"
    );
    assert_eq!(
        schema.pointer("/properties/id/maxLength"),
        Some(&json!(255)),
        "id must carry the base contract's length bound:\n{schema:#}"
    );
    assert_eq!(
        schema.pointer("/properties/metadata/type"),
        Some(&json!("object"))
    );
    assert_eq!(
        schema.pointer("/properties/type/x-gts-ref"),
        Some(&json!(toolkit_gts::gts_id!("cf.core.lic.subj.v1~"))),
        "subject `type` must ref the Subject base:\n{schema:#}"
    );
}

#[test]
fn derived_resource_refines_metadata_schema() {
    let meta = derived_metadata_subschema(&model_usage_schema());
    assert_eq!(meta.get("type"), Some(&json!("object")));
    assert_eq!(
        meta.pointer("/properties/model_vendor/type"),
        Some(&json!("string"))
    );
    assert_eq!(
        meta.pointer("/properties/model_name/type"),
        Some(&json!("string"))
    );
    assert_eq!(
        meta.get("required"),
        Some(&json!(["model_vendor", "model_name"]))
    );
}

#[test]
fn derived_subject_refines_metadata_schema() {
    let meta = derived_metadata_subschema(&user_subject_schema());
    assert_eq!(meta.get("type"), Some(&json!("object")));
    assert_eq!(
        meta.pointer("/properties/category/type"),
        Some(&json!("string"))
    );
    assert_eq!(meta.get("required"), Some(&json!(["category"])));
}

#[test]
fn derived_contract_does_not_add_top_level_licensing_properties() {
    let schema = model_usage_schema();
    let overlay = schema
        .pointer("/allOf/1/properties")
        .expect("derived overlay properties");
    let keys: Vec<&str> = overlay
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["metadata"],
        "derived contract must refine only `metadata`:\n{schema:#}"
    );
}
