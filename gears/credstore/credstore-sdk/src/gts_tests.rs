//! Unit tests for the registered secret-type GTS seed schemas.

use crate::models::SharingMode;
use crate::types::SECRET_TYPE_CATALOG;

/// The registered base traits schema must stay free of `$ref`/`oneOf`
/// shapes (the GTS trait validator resolves neither — a `oneOf` enum
/// breaks types-registry boot validation) and its `allow_sharing` enum
/// must match the serde labels of [`SharingMode`].
#[test]
fn sharing_modes_schema_matches_enum() {
    let registered = toolkit_gts::all_inventory_type_schemas().expect("inventory schemas parse");
    let want_id = serde_json::json!(format!("gts://{}", super::CREDENTIAL_RESOURCE_TYPE));
    let base = registered
        .iter()
        .find(|s| s["$id"] == want_id)
        .expect("secret base type registered");
    let traits_schema = &base["x-gts-traits-schema"];
    let rendered = traits_schema.to_string();
    assert!(
        !rendered.contains("$ref") && !rendered.contains("oneOf"),
        "traits schema must stay $ref/oneOf-free for the GTS validator: {rendered}"
    );
    let labels: Vec<serde_json::Value> = [
        SharingMode::Private,
        SharingMode::Tenant,
        SharingMode::Shared,
    ]
    .iter()
    .map(|m| serde_json::to_value(m).expect("serializes"))
    .collect();
    assert_eq!(
        traits_schema["properties"]["allow_sharing"]["items"]["enum"],
        serde_json::Value::Array(labels),
        "allow_sharing enum drifted from SharingMode's serde labels"
    );
}

/// The secret base type is a pure trait/schema carrier — secrets are always
/// typed by a derived id, never by the bare base — so its registered schema
/// must be marked abstract. It must still carry `x-gts-traits` values — not
/// for validation (a value-less abstract base would validate) but as the
/// chain-merge inherited defaults (leaf wins, base fills the rest): a derived
/// type declaring only some traits inherits the generic values for the rest;
/// without them the holes fall back to the trait-schema defaults
/// (`allow_sharing: []`), silently making sparse custom types unwritable.
#[test]
fn secret_base_type_is_abstract_and_still_carries_traits() {
    let registered = toolkit_gts::all_inventory_type_schemas().expect("inventory schemas parse");
    let want_id = serde_json::json!(format!("gts://{}", super::CREDENTIAL_RESOURCE_TYPE));
    let base = registered
        .iter()
        .find(|s| s["$id"] == want_id)
        .expect("secret base type registered");
    assert_eq!(
        base["x-gts-abstract"],
        serde_json::json!(true),
        "secret base must be x-gts-abstract: true (pure trait carrier)"
    );
    assert!(
        base["x-gts-traits"].is_object(),
        "abstract base must still declare x-gts-traits (inherited generic defaults for sparse descendants)"
    );
}

/// Every catalog entry must be backed by a registered GTS type schema
/// whose `x-gts-traits` mirror the catalog descriptor — a divergence
/// between the compiled-in traits and the registered types trips here.
#[test]
fn every_catalog_type_has_a_registered_schema_with_matching_traits() {
    let registered = toolkit_gts::all_inventory_type_schemas().expect("inventory schemas parse");
    for d in SECRET_TYPE_CATALOG {
        let want_id = serde_json::json!(format!("gts://{}", d.gts_id));
        let schema = registered
            .iter()
            .find(|s| s["$id"] == want_id)
            .unwrap_or_else(|| panic!("no registered schema for {}", d.name));
        let traits = &schema["x-gts-traits"];
        assert_eq!(traits["expirable"], serde_json::json!(d.expirable));
        assert_eq!(traits["utf8_only"], serde_json::json!(d.utf8_only));
        assert_eq!(
            traits["allow_sharing"].as_array().map(Vec::len),
            Some(d.allow_sharing.len())
        );
        match d.value_schema {
            Some(src) => {
                let want: serde_json::Value =
                    serde_json::from_str(src).expect("catalog value_schema parses");
                assert_eq!(
                    traits["value_schema"], want,
                    "{}: value_schema trait",
                    d.name
                );
            }
            None => assert!(
                traits.get("value_schema").is_none(),
                "{}: unexpected value_schema trait",
                d.name
            ),
        }
    }
}

/// The registered trait values must deserialize into the runtime traits
/// carrier — this is exactly what the gear's registry-driven resolver
/// does with `effective_traits()`, so a shape drift trips here first.
#[test]
fn registered_traits_deserialize_into_secret_type_traits() {
    let registered = toolkit_gts::all_inventory_type_schemas().expect("inventory schemas parse");
    for d in SECRET_TYPE_CATALOG {
        let want_id = serde_json::json!(format!("gts://{}", d.gts_id));
        let schema = registered
            .iter()
            .find(|s| s["$id"] == want_id)
            .unwrap_or_else(|| panic!("no registered schema for {}", d.name));
        let traits: super::SecretTypeTraits =
            serde_json::from_value(schema["x-gts-traits"].clone())
                .unwrap_or_else(|e| panic!("{}: traits must deserialize: {e}", d.name));
        for mode in d.allow_sharing {
            assert!(traits.allows_sharing(*mode), "{}: {mode:?}", d.name);
        }
        assert_eq!(traits.expirable, d.expirable, "{}", d.name);
        assert_eq!(traits.utf8_only, d.utf8_only, "{}", d.name);
        assert_eq!(
            traits.max_size_bytes,
            d.max_size_bytes
                .map(|n| u64::try_from(n).expect("catalog size fits u64")),
            "{}",
            d.name
        );
        assert_eq!(traits.value_schema.is_some(), d.value_schema.is_some());
    }
}
