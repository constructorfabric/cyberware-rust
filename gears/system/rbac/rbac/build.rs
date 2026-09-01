//! Vendored GTS-schema `$id` drift check.
//!
//! `src/module.rs` declares the canonical GTS ids for the two RBAC entity
//! schemas as `pub const` strings and `include_str!`s the
//! `schemas/*.v1.schema.json` files. Those ids and the schemas' `$id` fields
//! are kept in lock-step by hand, so this script parses each JSON file at
//! compile time and fails the build when an `$id` does not match its constant.
//! Without the check a stale `$id` ships silently and surfaces only at platform
//! startup, when `Gear::register_schemas` reaches the upstream registry.
//!
//! A build script rather than a proc-macro or a `const fn` parser: `serde_json`
//! is not const, and a plain compile error is the right failure mode.
//!
//! The same check pins the documentation mirror under `../docs/schemas/`
//! byte-for-byte against the vendored copies, so DESIGN readers are never
//! served a contract the gear does not register.

// A build script's whole job is to fail the build loud when an
// invariant breaks, so `panic!` / `.expect(...)` are the idiomatic
// failure mode here — the workspace `clippy::panic` / `expect_used`
// lints target runtime production code, not build scripts.
#![allow(clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::Path;

const ROLE_DEFINITION_GTS_ID: &str = "gts://gts.cf.core.rbac.role_definition.v1~";
const ROLE_ASSIGNMENT_GTS_ID: &str = "gts://gts.cf.core.rbac.role_assignment.v1~";

fn main() {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");

    check_schema_id(
        &manifest_dir,
        "schemas/role_definition.v1.schema.json",
        ROLE_DEFINITION_GTS_ID,
    );
    check_schema_id(
        &manifest_dir,
        "schemas/role_assignment.v1.schema.json",
        ROLE_ASSIGNMENT_GTS_ID,
    );

    for schema in DOC_MIRRORED_SCHEMAS {
        check_doc_mirror(&manifest_dir, schema);
    }
}

/// Schema files that exist twice: vendored under `schemas/` (compiled in
/// and registered at startup) and mirrored under `../docs/schemas/` (the
/// copy DESIGN links to). Every entry is compared byte-for-byte.
const DOC_MIRRORED_SCHEMAS: &[&str] = &[
    "role_definition.v1.schema.json",
    "role_assignment.v1.schema.json",
    "role_definition_created.v1.schema.json",
    "role_definition_updated.v1.schema.json",
    "role_definition_deleted.v1.schema.json",
    "role_assignment_created.v1.schema.json",
    "role_assignment_deleted.v1.schema.json",
];

/// Assert that `docs/schemas/<file_name>` is byte-identical to the
/// vendored `schemas/<file_name>`. Emits `cargo:rerun-if-changed` for
/// both so editing either side re-runs the check.
fn check_doc_mirror(manifest_dir: &str, file_name: &str) {
    let vendored = Path::new(manifest_dir).join("schemas").join(file_name);
    let mirrored = Path::new(manifest_dir)
        .join("../docs/schemas")
        .join(file_name);

    println!("cargo:rerun-if-changed={}", vendored.display());
    println!("cargo:rerun-if-changed={}", mirrored.display());

    let read = |path: &Path| {
        fs::read(path).unwrap_or_else(|err| {
            panic!(
                "rbac build.rs: failed to read schema `{}`: {err}",
                path.display()
            )
        })
    };

    assert!(
        read(&vendored) == read(&mirrored),
        "rbac build.rs: `docs/schemas/{file_name}` has drifted from the vendored \
         `schemas/{file_name}` — the docs copy is a mirror, so re-copy the \
         vendored file instead of editing it in place"
    );
}

/// Read `relative_path` under `manifest_dir`, parse it as JSON, and
/// assert that its top-level `$id` field equals `expected_id`. Emits a
/// `cargo:rerun-if-changed` directive so a schema edit re-runs the
/// check.
fn check_schema_id(manifest_dir: &str, relative_path: &str, expected_id: &str) {
    let path = Path::new(manifest_dir).join(relative_path);
    println!("cargo:rerun-if-changed={}", path.display());

    let bytes = fs::read(&path).unwrap_or_else(|err| {
        panic!(
            "rbac build.rs: failed to read vendored schema `{}`: {err}",
            path.display()
        )
    });

    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|err| {
        panic!(
            "rbac build.rs: vendored schema `{}` is not valid JSON: {err}",
            path.display()
        )
    });

    let actual_id = parsed
        .get("$id")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            panic!(
                "rbac build.rs: vendored schema `{}` is missing a top-level `$id` string field",
                path.display()
            )
        });

    assert_eq!(
        actual_id,
        expected_id,
        "rbac build.rs: vendored schema `{}` has $id `{actual_id}` but src/module.rs \
         expects `{expected_id}` — keep them in lock-step or platform startup \
         will fail at `register_schemas`",
        path.display()
    );
}
