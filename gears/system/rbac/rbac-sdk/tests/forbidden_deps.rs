//! Build-time forbidden-deps assertion for `rbac-sdk`.
//!
//! Enforces the "infrastructure-free SDK crate" contract by asserting that
//! `cargo tree -p cf-gears-rbac-sdk` does not pull in `sea-orm`, `sea-orm-migration`,
//! `sqlx`, `cf-gears-toolkit-db`, or `reqwest`. (`axum` / `hyper` are
//! permitted because they flow in transitively via `cf-gears-toolkit`.)

use std::process::Command;

const FORBIDDEN: &[&str] = &[
    "sea-orm",
    "sea-orm-migration",
    "sqlx",
    "cf-gears-toolkit-db",
    "reqwest",
];

/// Cargo package name (not the `rbac_sdk` lib-target name) — `cargo tree -p`
/// resolves package ids.
const SDK_PACKAGE: &str = "cf-gears-rbac-sdk";

#[test]
fn rbac_service_sdk_closure_has_no_forbidden_infrastructure_crates() {
    // `--prefix none` yields a flat list of `name vX.Y.Z` lines.
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--package",
            SDK_PACKAGE,
            "--prefix",
            "none",
            "--no-dedupe",
        ])
        .output()
        .expect("`cargo tree` MUST succeed; cargo is on PATH because it launched this test");

    assert!(
        output.status.success(),
        "`cargo tree -p {SDK_PACKAGE}` exited non-zero: stdout={stdout} stderr={stderr}",
        stdout = String::from_utf8_lossy(&output.stdout),
        stderr = String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8(output.stdout).expect("`cargo tree` output MUST be UTF-8");

    // Collect leading crate names; a `Vec` preserves first-appearance order
    // in the failure message for debugging which dep introduced a violation.
    let crate_names: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .collect();

    let mut violations: Vec<&str> = Vec::new();
    for forbidden in FORBIDDEN {
        if crate_names.iter().any(|name| name == forbidden) {
            violations.push(forbidden);
        }
    }

    assert!(
        violations.is_empty(),
        "rbac-sdk closure leaked forbidden infrastructure crate(s): {violations:?}\n\
         Requirement: 'Infrastructure-free SDK crate'.\n\
         Forbidden set: {FORBIDDEN:?}\n\
         Note: axum/hyper transitively via cf-gears-toolkit are NOT violations."
    );
}

/// Every role-definition-CRUD error variant constructs against SDK-only types
/// AND renders a distinct, non-empty message.
///
/// The SDK-only part is enforced by the `cargo tree` closure check above; this
/// covers what that cannot see. It used to construct each error into `_err` and
/// assert nothing, so a variant whose `Display` was empty — or copy-pasted from
/// its neighbour, which is how these messages get written — passed silently.
#[test]
fn role_definition_crud_error_variants_render_distinct_messages() {
    use rbac_sdk::error::RbacServiceError;
    use std::collections::BTreeSet;
    use uuid::Uuid;

    let id = Uuid::nil();
    let errors = vec![
        RbacServiceError::role_definition_name_taken("Auditor", Some(id)),
        RbacServiceError::role_definition_name_reserved_by_builtin("Owner"),
        RbacServiceError::role_definition_assignments_exist(id),
        RbacServiceError::built_in_role_not_modifiable(id),
        RbacServiceError::invalid_permission_rule("permissions[0]"),
        RbacServiceError::immutable_field_rejected("is_built_in"),
        RbacServiceError::owner_tenant_mismatch(),
        RbacServiceError::owner_tenant_required(),
        RbacServiceError::optimistic_concurrency_missing(),
        RbacServiceError::optimistic_concurrency_stale(
            "1970-01-01T00:00:00.000000Z:00000000-0000-0000-0000-000000000000",
        ),
    ];

    let mut rendered = BTreeSet::new();
    for err in &errors {
        let message = err.to_string();
        assert!(
            !message.trim().is_empty(),
            "every variant must render a message; {err:?} rendered empty"
        );
        assert!(
            rendered.insert(message.clone()),
            "two variants render the same message ({message:?}); a caller \
             reading the log cannot tell them apart"
        );
    }
    assert_eq!(rendered.len(), errors.len());
}
