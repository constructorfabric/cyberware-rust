//! Generates `docs/schemas/` from the declarations in [`crate::gts`], and holds
//! the worked examples to them.
//!
//! The same function produces a committed file and the schema registered into
//! `types-registry`, so a hand-edited file would document a contract the broker
//! does not register. Regenerate rather than edit:
//!
//! ```text
//! GTS_REGEN=1 cargo test -p cf-gears-event-broker-sdk --lib gts_tests -- --ignored
//! ```
//!
//! The two byte-comparison tests are `#[ignore]`d, hence `--ignored`: the emitted
//! formatting belongs to the gts library and changes with it, so comparing bytes
//! gates on a third party's output. Drop the `GTS_REGEN=1` to compare instead of
//! rewrite. The contract stays covered by the conformance tests below, which
//! validate behaviour against the freshly emitted base.

use crate::gts::{EventV1, TopicV1};

/// Path of a gear-committed schema file, relative to the crate root.
fn schema_path(file: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../docs/schemas")
        .join(file)
}

fn emitted(schema: &str) -> serde_json::Value {
    serde_json::from_str(schema).expect("emitted GTS schema is valid JSON")
}

fn committed(file: &str) -> serde_json::Value {
    let path = schema_path(file);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).expect("committed schema is valid JSON")
}

/// Asserts the committed document at `file` equals `generated`, or rewrites it
/// when `GTS_REGEN` is set. One switch for every document, so a regeneration can
/// never be partial. Two-space JSON with a trailing newline, matching every
/// other committed schema in the repository.
fn committed_matches(file: &str, generated: &serde_json::Value, source: &str) {
    if std::env::var_os("GTS_REGEN").is_some() {
        let path = schema_path(file);
        let body = serde_json::to_string_pretty(generated).expect("serialise schema");
        std::fs::write(&path, format!("{body}\n"))
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        return;
    }
    assert_eq!(
        &committed(file),
        generated,
        "{file} is out of date; regenerate it from {source} with \
         `GTS_REGEN=1 cargo test -p cf-gears-event-broker-sdk --lib gts_tests -- --ignored`"
    );
}

#[test]
#[ignore = "the emitted formatting is the gts library's to choose; run manually with `-- --ignored`"]
fn topic_base_schema_matches_committed_file() {
    committed_matches(
        "gts.cf.core.events.topic.v1~.schema.json",
        &emitted(&TopicV1::gts_schema_with_refs_as_string()),
        "`crate::gts::TopicV1`",
    );
}

#[test]
#[ignore = "the emitted formatting is the gts library's to choose; run manually with `-- --ignored`"]
fn event_base_schema_matches_committed_file() {
    committed_matches(
        "gts.cf.core.events.event.v1~.schema.json",
        &emitted(&EventV1::gts_schema_with_refs_as_string()),
        "`crate::gts::EventV1`",
    );
}

#[test]
fn only_the_two_base_types_are_declared() {
    // Inventory iteration order is link order, not declaration order.
    let mut ids: Vec<&str> = toolkit_gts::inventory::iter::<toolkit_gts::InventoryTypeSchema>
        .into_iter()
        .map(|e| e.type_id)
        .filter(|id| id.contains(".events."))
        .collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![
            toolkit_gts::gts_id!("cf.core.events.event.v1~"),
            toolkit_gts::gts_id!("cf.core.events.topic.v1~"),
        ],
        "the broker declares exactly the topic and event base types"
    );
}

// ---------------------------------------------------------------------------
// Worked examples
// ---------------------------------------------------------------------------
//
// Both documents under `docs/schemas/examples/` are derived type schemas, which
// `#[gts_type_schema]` cannot declare here: its derived form fills a generic slot
// on its base (the `PluginV1<P>` pattern) and neither of these bases is generic.
// So they stay hand-written JSON, and the checks below stand in for generation -
// an example that stops deriving from its base, or claims a trait the base does
// not declare, fails the build. `fabrikam` is the vendor this repository uses for
// a third party (`docs/TOOLKIT_PLUGINS.md`), and is one of the four registered
// with `gts-validator`.

/// Holds a committed derived-type example to the base it claims to derive from:
/// it must `$ref` that base, its trait values must satisfy the base's generated
/// `x-gts-traits-schema`, and it must declare no trait the base does not.
///
/// The examples stay JSON documents rather than Rust declarations because
/// `#[gts_type_schema]`'s derived form fills a generic slot on its base (the
/// `PluginV1<P>` pattern), and neither of these bases is generic.
fn example_conforms_to_base(example_file: &str, base: &serde_json::Value, base_uri: &str) {
    let example = committed(example_file);

    assert_eq!(
        example["allOf"][0]["$ref"],
        serde_json::json!(base_uri),
        "{example_file} must derive from {base_uri}"
    );

    let trait_schema = &base["x-gts-traits-schema"];
    let traits = &example["x-gts-traits"];
    let validator = jsonschema::validator_for(trait_schema).expect("base trait schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(traits)
        .map(|e| e.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "{example_file} traits violate the base trait schema: {errors:?}"
    );

    let declared: Vec<&str> = trait_schema["properties"]
        .as_object()
        .expect("trait schema declares properties")
        .keys()
        .map(String::as_str)
        .collect();
    for key in traits.as_object().expect("traits is an object").keys() {
        assert!(
            declared.contains(&key.as_str()),
            "{example_file} declares trait `{key}`, which the base does not"
        );
    }
}

#[test]
fn committed_example_event_type_conforms_to_the_generated_base() {
    example_conforms_to_base(
        "examples/gts.cf.core.events.event.v1~fabrikam.shop.orders.order_placed.v1~.schema.json",
        &emitted(&EventV1::gts_schema_with_refs_as_string()),
        toolkit_gts::gts_uri!("cf.core.events.event.v1~"),
    );
}

/// The committed topic example is an instance document, so what holds it to the
/// base is ordinary JSON Schema validation rather than the trait-derivation
/// checks a derived type gets: a topic has no traits and derives from nothing.
#[test]
fn committed_example_topic_validates_against_the_generated_base() {
    let base = emitted(&TopicV1::gts_schema_with_refs_as_string());
    let example = committed("examples/gts.cf.core.events.topic.v1~fabrikam.shop._.orders.v1.json");

    let validator = jsonschema::validator_for(&base).expect("base schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(&example)
        .map(|err| err.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "the example topic does not validate against its base: {errors:?}"
    );

    assert!(
        example.get("allOf").is_none() && example.get("x-gts-traits").is_none(),
        "a topic is an instance: it derives from nothing and declares no traits"
    );
}

#[test]
fn a_topic_missing_a_required_property_is_rejected() {
    let base = emitted(&TopicV1::gts_schema_with_refs_as_string());
    let validator = jsonschema::validator_for(&base).expect("base schema compiles");

    // `description` is required: a stream nobody can identify is not registrable.
    let without_description = serde_json::json!({
        "id": "gts.cf.core.events.topic.v1~fabrikam.shop._.orders.v1"
    });
    assert!(
        validator.iter_errors(&without_description).next().is_some(),
        "a topic without a description must not validate"
    );
}

/// A derived event type may only *narrow* its base. An empty payload schema
/// admits any JSON value, which is wider than the base's `["object", "null"]`,
/// and `types-registry` refuses the document with "Schema at `$.data` changes
/// type incompatibly" - which stops the whole gear from booting, since the
/// registry commits its configuration-seeded entities during `post_init`.
///
/// That failure reached a running process once because nothing here covered
/// this helper at all: every fixture and the mock build their event types
/// through it, and none of them validates against the base.
#[test]
fn an_unconstrained_payload_renders_as_the_bases_own_type_not_as_any() {
    let base_data =
        committed("gts.cf.core.events.event.v1~.schema.json")["properties"]["data"]["type"].clone();

    let derived = crate::gts::derived_event_type_schema(
        "gts.cf.core.events.event.v1~example.eb.narrowing.foo.v1~",
        "gts.cf.core.events.topic.v1~example.eb.narrowing.topic.v1",
        serde_json::json!({}),
        &["gts.example.eb.narrowing.subject.v1~"],
    );

    let narrowing = &derived["allOf"][1]["properties"]["data"];
    assert_eq!(
        narrowing["type"], base_data,
        "stating no constraints means whatever the base allows, so the member \
         carries the base's own type rather than an empty schema"
    );
    assert_ne!(
        *narrowing,
        serde_json::json!({}),
        "an empty schema here is what registration refuses"
    );
}

/// The other half: a payload contract the caller supplies is written through
/// untouched, since narrowing is exactly what it is for.
#[test]
fn a_supplied_payload_contract_is_written_through_unchanged() {
    let contract = serde_json::json!({
        "type": "object",
        "required": ["order_id"],
        "properties": { "order_id": { "type": "string", "format": "uuid" } },
    });

    let derived = crate::gts::derived_event_type_schema(
        "gts.cf.core.events.event.v1~example.eb.narrowing.foo.v1~",
        "gts.cf.core.events.topic.v1~example.eb.narrowing.topic.v1",
        contract.clone(),
        &[],
    );

    assert_eq!(derived["allOf"][1]["properties"]["data"], contract);
}
