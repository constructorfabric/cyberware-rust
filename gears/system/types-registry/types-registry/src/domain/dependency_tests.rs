//! Unit tests for the pure edge extractor.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use gts::GtsId;
use serde_json::{Value, json};
use toolkit_gts::gts_id;

use super::{DependencyEdge, extract_edges, reference_targets};
use crate::domain::enums::DependencyKind;

const ROOT: &str = gts_id!("cf.core.example.root.v1~");
const DERIVED: &str = gts_id!("cf.core.example.root.v1~cf.core.example.leaf.v1~");
const INSTANCE: &str = gts_id!("cf.core.example.root.v1~cf.core.example.first.v1");
const OTHER: &str = gts_id!("cf.core.other.shape.v1~");
const THIRD: &str = gts_id!("cf.core.other.third.v1~");

fn id(s: &str) -> GtsId {
    GtsId::try_new(s).expect("the fixture identifier parses")
}

fn schema(gts_id: &str, body: Value) -> Value {
    let mut doc = json!({
        "$id": format!("gts://{gts_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
    });
    let Value::Object(extra) = body else {
        panic!("a schema body fixture must be an object");
    };
    for (key, value) in extra {
        doc[key] = value;
    }
    doc
}

fn edges(gts_id: &str, content: &Value) -> Vec<DependencyEdge> {
    extract_edges(&id(gts_id), content).expect("the fixture extracts")
}

fn targets_of(edges: &[DependencyEdge], kind: DependencyKind) -> Vec<String> {
    edges
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.target.clone())
        .collect()
}

#[test]
fn a_ref_outside_the_identifier_chain_is_a_schema_ref_edge() {
    let doc = schema(
        ROOT,
        json!({ "properties": { "shape": { "$ref": format!("gts://{OTHER}") } } }),
    );
    assert_eq!(
        targets_of(&edges(ROOT, &doc), DependencyKind::SchemaRef),
        vec![OTHER.to_owned()],
        "a `$ref` is the one edge kind no identifier implies",
    );
}

#[test]
fn refs_are_deduplicated_and_the_traits_schema_is_covered() {
    let doc = schema(
        ROOT,
        json!({
            "properties": {
                "a": { "$ref": format!("gts://{OTHER}") },
                "b": { "$ref": format!("gts://{OTHER}#/properties/inner") },
                "local": { "$ref": "#/$defs/Local" },
            },
            "$defs": { "Local": { "type": "string" } },
            "x-gts-traits-schema": {
                "type": "object",
                "properties": { "t": { "$ref": format!("gts://{THIRD}") } },
            },
        }),
    );
    let mut found = targets_of(&edges(ROOT, &doc), DependencyKind::SchemaRef);
    found.sort();
    assert_eq!(
        found,
        vec![OTHER.to_owned(), THIRD.to_owned()],
        "two references to one target are one edge; a local pointer is none, and \
         `x-gts-traits-schema` is part of the document",
    );
}

#[test]
fn a_malformed_ref_is_reported_rather_than_silently_dropped() {
    let doc = schema(ROOT, json!({ "properties": { "a": { "$ref": OTHER } } }));
    let err = extract_edges(&id(ROOT), &doc).expect_err("a bare-id ref is not extractable");
    assert_eq!(err.gts_id, ROOT);
}

#[test]
fn an_x_gts_ref_is_not_a_dependency_edge_in_any_of_its_forms() {
    for value in [
        OTHER,                // an exact identifier
        &format!("{OTHER}*"), // a wildcard pattern over it
        "gts.*",              // a pattern naming nothing valid
        "/$id",               // a GTS §9.6 relative pointer
    ] {
        let doc = schema(
            ROOT,
            json!({ "properties": { "role": { "type": "string", "x-gts-ref": value } } }),
        );
        assert!(
            edges(ROOT, &doc).is_empty(),
            "`x-gts-ref: {value}` must produce no edge: the keyword is enforced by \
             matching the value string, so it never consults the registry",
        );
    }
}

#[test]
fn an_x_gts_ref_inside_a_data_valued_keyword_is_data() {
    let doc = schema(
        ROOT,
        json!({ "properties": { "a": { "const": { "x-gts-ref": OTHER } } } }),
    );
    assert!(edges(ROOT, &doc).is_empty());
}

#[test]
fn a_derived_schema_edges_only_to_its_immediate_base() {
    let doc = schema(DERIVED, json!({}));
    assert_eq!(
        targets_of(&edges(DERIVED, &doc), DependencyKind::Derivation),
        vec![ROOT.to_owned()],
        "the chain above the base is reached by walking the base's own edge, not by \
         a second row from here",
    );
}

#[test]
fn a_first_generation_schema_has_no_derivation_edge() {
    let doc = schema(ROOT, json!({}));
    assert!(
        edges(ROOT, &doc).is_empty(),
        "nothing above a single segment"
    );
}

#[test]
fn an_instance_conforms_to_its_type_and_carries_nothing_else() {
    let value = json!({ "name": "first" });
    assert_eq!(
        edges(INSTANCE, &value),
        vec![DependencyEdge {
            kind: DependencyKind::InstanceOf,
            target: ROOT.to_owned(),
        }],
    );
}

#[test]
fn an_instance_values_ref_shaped_data_is_data_and_not_an_edge() {
    // Schema keywords inside an Instance value create no edges.
    let value = json!({
        "$ref": OTHER,
        "x-gts-ref": "not-an-identifier",
        "nested": { "$ref": format!("gts://{THIRD}") },
    });
    assert_eq!(
        edges(INSTANCE, &value),
        vec![DependencyEdge {
            kind: DependencyKind::InstanceOf,
            target: ROOT.to_owned(),
        }],
    );
}

#[test]
fn the_edge_kinds_over_their_fixtures() {
    struct Case {
        what: &'static str,
        gts_id: &'static str,
        content: Value,
        expected: Vec<(DependencyKind, &'static str)>,
    }

    let cases = vec![
        Case {
            what: "a derived schema with a `$ref`: both kinds a schema can carry, and the \
                   `x-gts-ref` beside them adding neither",
            gts_id: DERIVED,
            content: schema(
                DERIVED,
                json!({
                    "properties": {
                        "shape": { "$ref": format!("gts://{OTHER}") },
                        "role": { "type": "string", "x-gts-ref": THIRD },
                    },
                }),
            ),
            expected: vec![
                (DependencyKind::SchemaRef, OTHER),
                (DependencyKind::Derivation, ROOT),
            ],
        },
        Case {
            what: "a reference-free root schema has no edges of any kind",
            gts_id: ROOT,
            content: schema(
                ROOT,
                json!({ "properties": { "name": { "type": "string" } } }),
            ),
            expected: vec![],
        },
        Case {
            what: "an Instance of a derived type conforms to that type, not to the root",
            gts_id: gts_id!("cf.core.example.root.v1~cf.core.example.leaf.v1~cf.core.example.i.v1"),
            content: json!({ "name": "i" }),
            expected: vec![(DependencyKind::InstanceOf, DERIVED)],
        },
    ];

    for case in cases {
        let mut found: Vec<(DependencyKind, String)> = edges(case.gts_id, &case.content)
            .into_iter()
            .map(|e| (e.kind, e.target))
            .collect();
        found.sort();
        let mut expected: Vec<(DependencyKind, String)> = case
            .expected
            .into_iter()
            .map(|(kind, target)| (kind, target.to_owned()))
            .collect();
        expected.sort();
        assert_eq!(found, expected, "{}", case.what);
    }
}

#[test]
fn only_the_ref_targets_seed_the_closure() {
    // Only `$ref` targets require extra closure roots.
    let doc = schema(
        DERIVED,
        json!({
            "properties": {
                "shape": { "$ref": format!("gts://{OTHER}") },
                "role": { "type": "string", "x-gts-ref": THIRD },
            },
        }),
    );
    assert_eq!(
        reference_targets(&edges(DERIVED, &doc)),
        vec![OTHER.to_owned()],
    );
}
