#![allow(clippy::expect_used)]

use gts::{GTS_ID_PREFIX, GTS_ID_URI_PREFIX, GtsSchema, GtsStore};
use serde_json::Value;
use toolkit_gts::{InventoryInstance, InventoryTypeSchema};

use super::{
    CONSTRAINT_BASE, LEASE_RESOURCE, METRIC_BASE_TYPE, OPERATION_RESOURCE, OwnedDefinition,
    POLICY_RESOURCE, QUOTA_RESOURCE, QuotaEnforcementStoragePluginSpecV1, REQUEST_BASE,
    RESOURCE_BASE, SCOPE_TENANT, SCOPE_TYPE, SCOPE_USER, SUBJECT_BASE, owned_definitions,
};

/// The reviewed owner examples of ADR-0007: one complete `llm_gateway` set.
const EXAMPLES: [(&str, &str); 7] = [
    (
        "gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~",
        include_str!(
            "../../docs/schemas/examples/gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~.schema.json"
        ),
    ),
    (
        "gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.tenant.v1~",
        include_str!(
            "../../docs/schemas/examples/gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.tenant.v1~.schema.json"
        ),
    ),
    (
        "gts.cf.core.qe.request.v1~cf.genai.llm_gateway.token.v1~",
        include_str!(
            "../../docs/schemas/examples/gts.cf.core.qe.request.v1~cf.genai.llm_gateway.token.v1~.schema.json"
        ),
    ),
    (
        "gts.cf.core.qe.request.v1~cf.genai.llm_gateway.request_count.v1~",
        include_str!(
            "../../docs/schemas/examples/gts.cf.core.qe.request.v1~cf.genai.llm_gateway.request_count.v1~.schema.json"
        ),
    ),
    (
        "gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.token_constraint.v1~",
        include_str!(
            "../../docs/schemas/examples/gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.token_constraint.v1~.schema.json"
        ),
    ),
    (
        "gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.request_count_constraint.v1~",
        include_str!(
            "../../docs/schemas/examples/gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.request_count_constraint.v1~.schema.json"
        ),
    ),
    (
        "gts.cf.core.qe.res.v1~cf.genai.llm_gateway.model.v1~",
        include_str!(
            "../../docs/schemas/examples/gts.cf.core.qe.res.v1~cf.genai.llm_gateway.model.v1~.schema.json"
        ),
    ),
];

fn parse(raw: &str) -> Value {
    serde_json::from_str(raw).expect("reviewed schema file is valid JSON")
}

fn definitions() -> Vec<OwnedDefinition> {
    owned_definitions().expect("embedded documents parse")
}

#[test]
fn the_storage_plugin_spec_type_id_derives_from_the_toolkit_plugin_base() {
    let storage = QuotaEnforcementStoragePluginSpecV1::TYPE_ID;
    assert!(
        storage.starts_with("gts.cf.toolkit.plugins.plugin.v1~"),
        "{storage}"
    );
    assert!(storage.ends_with('~'));
}

#[test]
fn resource_ids_are_distinct_five_segment_type_ids() {
    let all = [
        QUOTA_RESOURCE,
        POLICY_RESOURCE,
        LEASE_RESOURCE,
        OPERATION_RESOURCE,
    ];
    for id in all {
        assert!(id.starts_with("gts.cf.qe.resource."), "{id}");
        assert!(id.ends_with(".v1~"), "{id}");
    }
    let mut sorted = all.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), all.len(), "resource ids must be unique");
}

#[test]
fn owned_definitions_are_the_seven_qe_definitions_in_registration_order() {
    let defs = definitions();
    let ids: Vec<&str> = defs.iter().map(|d| d.id).collect();
    assert_eq!(
        ids,
        [
            SUBJECT_BASE,
            RESOURCE_BASE,
            REQUEST_BASE,
            CONSTRAINT_BASE,
            SCOPE_TYPE,
            SCOPE_USER,
            SCOPE_TENANT
        ]
    );
    let owned_prefix = format!("{GTS_ID_PREFIX}cf.core.qe.");
    for def in &defs {
        assert!(
            def.id.starts_with(&owned_prefix),
            "{} is not a QE-owned definition",
            def.id
        );
    }
    assert!(
        !ids.contains(&METRIC_BASE_TYPE),
        "the metric base is registry-owned and never registered by QE"
    );
}

#[test]
fn every_embedded_document_carries_the_id_of_its_constant() {
    for def in definitions() {
        if def.id.ends_with('~') {
            let expected = format!("{GTS_ID_URI_PREFIX}{}", def.id);
            assert_eq!(def.document["$id"], expected, "{}", def.id);
            assert_eq!(
                def.document["$schema"], "http://json-schema.org/draft-07/schema#",
                "{} must declare the Draft-07 dialect",
                def.id
            );
        } else {
            assert_eq!(def.document["id"], def.id);
            assert_eq!(def.document["type"], SCOPE_TYPE);
        }
    }
}

#[test]
fn the_four_bases_are_abstract_and_the_scope_type_is_concrete() {
    for def in definitions().iter().filter(|d| d.id.ends_with('~')) {
        let is_abstract = def.document["x-gts-abstract"] == Value::Bool(true);
        assert_eq!(is_abstract, def.id != SCOPE_TYPE, "{}", def.id);
    }
}

#[test]
fn scope_instance_payloads_equal_the_documented_files() {
    let user = parse(include_str!(
        "../../docs/schemas/gts.cf.core.qe.scope.v1~cf.core.qe.user.v1.json"
    ));
    let tenant = parse(include_str!(
        "../../docs/schemas/gts.cf.core.qe.scope.v1~cf.core.qe.tenant.v1.json"
    ));
    let defs = definitions();
    let by_id = |id: &str| {
        defs.iter()
            .find(|d| d.id == id)
            .map(|d| d.document.clone())
            .expect("instance present")
    };
    assert_eq!(by_id(SCOPE_USER), user);
    assert_eq!(by_id(SCOPE_TENANT), tenant);
}

#[test]
fn the_inventory_carries_the_same_seven_definitions() {
    let defs = definitions();
    for def in &defs {
        if def.id.ends_with('~') {
            let entry = inventory::iter::<InventoryTypeSchema>
                .into_iter()
                .find(|e| e.type_id == def.id)
                .expect("type schema submitted to the inventory");
            assert_eq!(parse(&(entry.schema_fn)()), def.document, "{}", def.id);
        } else {
            let entry = inventory::iter::<InventoryInstance>
                .into_iter()
                .find(|e| e.instance_id == def.id)
                .expect("instance submitted to the inventory");
            assert_eq!(entry.type_id, SCOPE_TYPE);
            assert_eq!((entry.payload_fn)(), def.document, "{}", def.id);
        }
    }
}

/// A store holding the QE definitions and the `llm_gateway` examples: the
/// repository's semantic check of the reviewed schema files. The registry
/// validates with the same `gts` library at admission time.
fn store_with_examples() -> GtsStore {
    let mut store = GtsStore::new();
    for def in definitions().iter().filter(|d| d.id.ends_with('~')) {
        store
            .register_schema(def.id, &def.document)
            .expect("QE type schema registers");
    }
    for (id, raw) in EXAMPLES {
        store
            .register_schema(id, &parse(raw))
            .expect("example schema registers");
    }
    store
}

#[test]
fn the_qe_bases_validate_as_abstract_types_with_required_value_less_traits() {
    let mut store = store_with_examples();
    for base in [SUBJECT_BASE, RESOURCE_BASE, REQUEST_BASE, CONSTRAINT_BASE] {
        let resolved = store
            .validate_schema(base)
            .unwrap_or_else(|e| panic!("{base}: {e}"));
        assert!(resolved.is_abstract, "{base} is abstract");
    }
    let scope = store
        .validate_schema(SCOPE_TYPE)
        .expect("scope type validates");
    assert!(!scope.is_abstract, "the scope type is concrete");
}

#[test]
fn the_owner_examples_validate_as_concrete_derived_contracts_with_their_traits() {
    let mut store = store_with_examples();
    for (id, _) in EXAMPLES {
        let resolved = store
            .validate_schema(id)
            .unwrap_or_else(|e| panic!("{id}: {e}"));
        assert!(!resolved.is_abstract, "{id} is concrete");
        assert!(
            resolved.schema["$schema"].is_string(),
            "{id}: the resolved schema keeps its dialect"
        );
    }
    let user = store
        .validate_schema("gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~")
        .expect("user projection");
    assert_eq!(user.effective_traits["scope"], SCOPE_USER);
    assert_eq!(
        user.effective_traits["admitted_metrics"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    let token = store
        .validate_schema("gts.cf.core.qe.request.v1~cf.genai.llm_gateway.token.v1~")
        .expect("token request contract");
    assert_eq!(
        token.effective_traits["metric"],
        format!("{METRIC_BASE_TYPE}cf.qe.metric.ai_tokens_input.v1")
    );
    assert_eq!(
        token.effective_traits["constraint_contract"],
        "gts.cf.core.qe.constraint.v1~cf.genai.llm_gateway.token_constraint.v1~"
    );
}

#[test]
fn scope_instances_validate_against_the_scope_type_and_bases_take_no_instances() {
    let mut store = store_with_examples();
    for def in definitions().iter().filter(|d| !d.id.ends_with('~')) {
        store
            .validate_payload(SCOPE_TYPE, &def.document)
            .unwrap_or_else(|e| panic!("{}: {e}", def.id));
    }
    let err = store
        .validate_payload(SUBJECT_BASE, &serde_json::json!({ "type": SUBJECT_BASE }))
        .expect_err("an abstract base takes no direct instances");
    assert!(err.to_string().contains("abstract"), "{err}");
}

#[test]
fn the_token_request_envelope_validates_against_its_resolved_contract() {
    let mut store = store_with_examples();
    let contract = "gts.cf.core.qe.request.v1~cf.genai.llm_gateway.token.v1~";
    store
        .validate_payload(
            contract,
            &serde_json::json!({ "type": contract, "metadata": { "region": "eu-west-1" } }),
        )
        .expect("a conforming envelope validates");
    let missing_region = store
        .validate_payload(
            contract,
            &serde_json::json!({ "type": contract, "metadata": {} }),
        )
        .expect_err("region is required by the owner contract");
    assert!(
        missing_region.to_string().contains("region"),
        "{missing_region}"
    );
    let bare_metadata = store
        .validate_payload(contract, &serde_json::json!({ "region": "eu-west-1" }))
        .expect_err("the inner object alone is not the contract envelope");
    assert!(
        bare_metadata.to_string().contains("type"),
        "{bare_metadata}"
    );
}

/// The published crate cannot reach `docs/`, so it ships its own copies of
/// the five base schemas. This holds the copies to the reviewed files.
#[test]
fn the_shipped_base_schemas_are_byte_identical_to_the_reviewed_documents() {
    const PAIRS: [(&str, &str, &str); 5] = [
        (
            "gts.cf.core.qe.subj.v1~.schema.json",
            include_str!("../schemas/gts.cf.core.qe.subj.v1~.schema.json"),
            include_str!("../../docs/schemas/gts.cf.core.qe.subj.v1~.schema.json"),
        ),
        (
            "gts.cf.core.qe.res.v1~.schema.json",
            include_str!("../schemas/gts.cf.core.qe.res.v1~.schema.json"),
            include_str!("../../docs/schemas/gts.cf.core.qe.res.v1~.schema.json"),
        ),
        (
            "gts.cf.core.qe.request.v1~.schema.json",
            include_str!("../schemas/gts.cf.core.qe.request.v1~.schema.json"),
            include_str!("../../docs/schemas/gts.cf.core.qe.request.v1~.schema.json"),
        ),
        (
            "gts.cf.core.qe.constraint.v1~.schema.json",
            include_str!("../schemas/gts.cf.core.qe.constraint.v1~.schema.json"),
            include_str!("../../docs/schemas/gts.cf.core.qe.constraint.v1~.schema.json"),
        ),
        (
            "gts.cf.core.qe.scope.v1~.schema.json",
            include_str!("../schemas/gts.cf.core.qe.scope.v1~.schema.json"),
            include_str!("../../docs/schemas/gts.cf.core.qe.scope.v1~.schema.json"),
        ),
    ];
    for (name, shipped, reviewed) in PAIRS {
        assert_eq!(
            shipped, reviewed,
            "quota-enforcement-sdk/schemas/{name} drifted from docs/schemas/{name}"
        );
    }
}
