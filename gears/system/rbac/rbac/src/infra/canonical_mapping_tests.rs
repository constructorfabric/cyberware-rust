//! Pinning tests for the DB-error → [`DomainError`] classification
//! ladder.

#![allow(clippy::panic)]
//!
//! Lives in `infra/` so the test code can import `sea_orm::DbErr` and
//! `toolkit_db::DbError` directly — both forbidden inside `domain/` by
//! the layering rules. Each test drives one branch of the ladder so
//! that adding a new SQLSTATE class lands here as one additional
//! `#[test]` instead of being silently swallowed by the catch-all
//! `Internal` arm.

use sea_orm::{ConnAcquireErr, DbErr, RuntimeErr};
use toolkit_db::DbError;

use super::{ConstraintHint, classify_db_err_to_domain, extract_constraint_hint};
use crate::domain::error::DomainError;

// ---------------------------------------------------------------------
// SQLSTATE 40001 — serialization conflict (retry-budget-exhausted).
// ---------------------------------------------------------------------

#[test]
fn sqlstate_40001_maps_to_aborted_with_serialization_conflict_reason() {
    // Wrapped through `RuntimeErr::Internal` — matches the string-
    // detection path in `toolkit_db::contention::is_retryable_contention`
    // which the RBAC `is_serialization_failure` adapter probes.
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "error returned from database: error with SQLSTATE 40001".to_owned(),
    ));
    let mapped = classify_db_err_to_domain(db_err);
    match mapped {
        DomainError::Aborted { reason, detail } => {
            assert_eq!(reason, "SERIALIZATION_CONFLICT");
            assert!(
                detail.contains("serialization conflict"),
                "detail must name the conflict class, got: {detail}"
            );
        }
        other => panic!("expected Aborted, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// SQLSTATE 23505 — unique-constraint violation.
// ---------------------------------------------------------------------

#[test]
fn sqlstate_23505_postgres_message_maps_to_already_exists() {
    // Postgres error text — string-fallback path of
    // `toolkit_db::secure::is_unique_violation`.
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "duplicate key value violates unique constraint \"uq_role_name_per_tenant\"".to_owned(),
    ));
    assert!(matches!(
        classify_db_err_to_domain(db_err),
        DomainError::AlreadyExists { .. }
    ));
}

#[test]
fn unique_violation_sqlite_message_maps_to_already_exists() {
    // SQLite shape: lowercased substring `"unique constraint failed"`
    // — fallback in `is_unique_violation`.
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "UNIQUE constraint failed: role_definitions.name, role_definitions.owner_tenant_id"
            .to_owned(),
    ));
    assert!(matches!(
        classify_db_err_to_domain(db_err),
        DomainError::AlreadyExists { .. }
    ));
}

/// An unstructured unique violation whose `DETAIL` value embeds the
/// digits `40001` MUST still map to `AlreadyExists`, NOT be mis-routed to a
/// retryable `Aborted` by the bare-substring `40001` contention probe that
/// runs first in the ladder. (`is_serialization_failure` now classifies a
/// known constraint violation before the contention probe.)
#[test]
fn unique_violation_with_40001_in_detail_value_is_not_serialization_conflict() {
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "duplicate key value violates unique constraint \"uq_assignment\"; \
         SQLSTATE 23505; DETAIL: Key (principal_id)=(svc-principal-40001) already exists."
            .to_owned(),
    ));
    match classify_db_err_to_domain(db_err) {
        DomainError::AlreadyExists { .. } => {}
        other => panic!(
            "a dup-key whose value contains '40001' MUST map to AlreadyExists, not {other:?}"
        ),
    }
}

// ---------------------------------------------------------------------
// SQLSTATE 23503 — foreign-key violation (unattributed).
// ---------------------------------------------------------------------

#[test]
fn sqlstate_23503_maps_to_conflict_not_internal() {
    // Regression guard: an unattributed FK violation MUST surface as
    // 409 (Conflict), not 500 (Internal). Repo-level `map_db_err` claims
    // RBAC-specific FK violations (e.g. `role_assignments_role_definition_id_fkey`)
    // before delegating; the classifier sees only unattributed FKs.
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "insert or update on table \"x\" violates foreign key constraint \"unknown_fk\"".to_owned(),
    ));
    match classify_db_err_to_domain(db_err) {
        DomainError::Conflict { detail } => {
            assert!(
                detail.contains("referential integrity"),
                "detail must name the FK class, got: {detail}"
            );
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// SQLSTATE 23514 — CHECK constraint violation.
// ---------------------------------------------------------------------

#[test]
fn sqlstate_23514_maps_to_validation_400_not_internal_500() {
    // DB-side `CHECK` predicates are the last line of defence behind the
    // service-layer validators. Routing them to `Validation` keeps the
    // public envelope at HTTP 400 (client can retry-correct), instead
    // of collapsing to HTTP 500 (operator-only failure).
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "new row for relation \"role_definitions\" violates check constraint \"ck_x\"".to_owned(),
    ));
    assert!(matches!(
        classify_db_err_to_domain(db_err),
        DomainError::Validation { .. }
    ));
}

// ---------------------------------------------------------------------
// Availability — connection acquire / connection-level outage.
// ---------------------------------------------------------------------

#[test]
fn pool_acquire_timeout_maps_to_service_unavailable() {
    let db_err = DbErr::ConnectionAcquire(ConnAcquireErr::Timeout);
    assert!(matches!(
        classify_db_err_to_domain(db_err),
        DomainError::ServiceUnavailable { .. }
    ));
}

#[test]
fn dberror_io_routes_to_service_unavailable() {
    // Regression guard: a transient IO outage (TCP reset, "connection
    // refused", etc.) MUST surface as 503 (ServiceUnavailable), not 500
    // (Internal). A non-`Sea` arm in `From<DbError>` that falls through to
    // `Internal` loses the availability signal.
    let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
    let lifted: DomainError = DbError::Io(io_err).into();
    assert!(matches!(lifted, DomainError::ServiceUnavailable { .. }));
}

// ---------------------------------------------------------------------
// Internal — unclassified fallback.
// ---------------------------------------------------------------------

#[test]
fn unclassified_dberr_falls_through_to_internal_with_redacted_diagnostic() {
    let db_err = DbErr::Custom("synthetic unrecognised driver error".to_owned());
    match classify_db_err_to_domain(db_err) {
        DomainError::Internal { diagnostic, cause } => {
            assert!(
                diagnostic.contains("unclassified database error"),
                "diagnostic must label this as the catch-all, got: {diagnostic}"
            );
            assert!(
                cause.is_none(),
                "classify_db_err_to_domain does not chain the raw DbErr; \
                 cause is reserved for the `From<DbError>` IO path"
            );
        }
        other => panic!("expected Internal, got {other:?}"),
    }
}

#[test]
fn dberror_sea_routes_through_classifier() {
    // `From<DbError> for DomainError`: `DbError::Sea(_)` delegates to
    // the classifier. An unrecognised inner `DbErr` therefore lands in
    // `Internal` exactly like a direct `classify_db_err_to_domain` call.
    let lifted: DomainError = DbError::Sea(DbErr::Custom("any".into())).into();
    assert!(matches!(lifted, DomainError::Internal { .. }));
}

// ---------------------------------------------------------------------
// ConstraintHint — refinement API used by repos.
// ---------------------------------------------------------------------

/// Helper: build a hint directly from in-test inputs. Side-steps
/// `extract_constraint_hint` because constructing a real
/// `SqlErr::UniqueConstraintViolation` requires a sqlx-backed `DbErr`
/// that we cannot synthesise without a live driver.
fn hint(structured_name: Option<&str>, raw_message: &str) -> ConstraintHint {
    ConstraintHint {
        structured_name: structured_name.map(str::to_owned),
        raw_message: raw_message.to_owned(),
    }
}

#[test]
fn constraint_hint_matches_prefers_structured_name_over_message() {
    let h = hint(
        Some("uq_role_name_builtin"),
        "totally unrelated message text",
    );
    assert!(h.matches("uq_role_name_builtin", &["name"]));
}

#[test]
fn constraint_hint_matches_rejects_when_structured_name_disagrees() {
    let h = hint(
        Some("some_other_constraint"),
        "the message mentions uq_role_name_builtin",
    );
    assert!(!h.matches("uq_role_name_builtin", &["name"]));
}

#[test]
fn constraint_hint_matches_pg_message_fallback_when_structured_absent() {
    let h = hint(
        None,
        "duplicate key value violates unique constraint \"uq_role_name_builtin\"",
    );
    assert!(h.matches("uq_role_name_builtin", &["name"]));
}

#[test]
fn constraint_hint_matches_sqlite_column_set_requires_every_column() {
    // SQLite per-tenant uniqueness message mentions both columns; the
    // matcher for the per-tenant constraint expects both, so it fires.
    let h = hint(
        None,
        "UNIQUE constraint failed: role_definitions.name, role_definitions.owner_tenant_id",
    );
    assert!(h.matches("uq_role_name_per_tenant", &["name", "owner_tenant_id"]));
    // The built-in matcher (just `["name"]`) ALSO fires here because
    // "name" appears in the message — that's why the repo's refinement
    // checks the more-specific per-tenant constraint first.
    assert!(h.matches("uq_role_name_builtin", &["name"]));
}

#[test]
fn constraint_hint_matches_sqlite_rejects_when_required_column_missing() {
    let h = hint(None, "UNIQUE constraint failed: role_definitions.name");
    // Per-tenant matcher needs both columns; one is missing → rejects.
    assert!(!h.matches("uq_role_name_per_tenant", &["name", "owner_tenant_id"]));
}

#[test]
fn constraint_hint_extract_quoted_value_parses_canonical_pg_detail() {
    let h = hint(
        Some("uq_role_name_per_tenant"),
        "Key (name, owner_tenant_id)=(Auditor, 11111111-1111-1111-1111-111111111111) \
         already exists.",
    );
    assert_eq!(h.extract_quoted_value().as_deref(), Some("Auditor"));
}

#[test]
fn constraint_hint_extract_quoted_value_parses_single_column_pg_detail() {
    let h = hint(
        Some("uq_role_name_builtin"),
        "Key (name)=(Owner) already exists.",
    );
    assert_eq!(h.extract_quoted_value().as_deref(), Some("Owner"));
}

#[test]
fn constraint_hint_extract_quoted_value_returns_none_on_open_value_segment() {
    // `)=(` present but the value never closes with `,` or `)`.
    let h = hint(None, "Key (name)=(Owner-no-closing");
    assert!(h.extract_quoted_value().is_none());
}

#[test]
fn constraint_hint_extract_quoted_value_returns_none_when_no_key_segment() {
    let h = hint(None, "some unrelated error message");
    assert!(h.extract_quoted_value().is_none());
}

#[test]
fn extract_constraint_hint_returns_none_for_non_sql_dberr() {
    // `DbErr::Custom` carries no SqlErr discriminator — no hint is
    // available, and the caller falls back to the generic classifier
    // outcome.
    let err = DbErr::Custom("synthetic".to_owned());
    assert!(extract_constraint_hint(&err).is_none());
}

#[test]
fn extract_constraint_hint_returns_none_for_runtime_internal_dberr() {
    // `RuntimeErr::Internal` is a string-wrapped error from the driver;
    // sea-orm only attaches a `SqlErr` discriminator when the underlying
    // wrapper is `RuntimeErr::SqlxError`.
    let err = DbErr::Exec(RuntimeErr::Internal("synthetic".to_owned()));
    assert!(extract_constraint_hint(&err).is_none());
}

/// The `PostgreSQL` 18 shape: an `ON DELETE RESTRICT` violation arrives as
/// SQLSTATE `23001`, which `sea_orm` does not type, so `sql_err()` is `None`
/// and the typed fast path cannot see it.
///
/// Regression guard. Without the untyped fallback in
/// `extract_constraint_hint`, deleting a role definition that still has
/// assignments degraded from the typed `RoleDefinitionAssignmentsExist` (a 409
/// naming the cause) to a bare referential-integrity `Conflict` — silently, and
/// only on `PostgreSQL` 18. The Docker-gated suite catches it too, but that
/// suite does not run on every push.
#[test]
fn extract_constraint_hint_recovers_an_untyped_restrict_violation() {
    let err = DbErr::Exec(RuntimeErr::Internal(
        "error returned from database: (23001) update or delete on table \"role_definitions\" \
         violates foreign key constraint \"role_assignments_role_definition_id_fkey\" on table \
         \"role_assignments\""
            .to_owned(),
    ));
    let hint = extract_constraint_hint(&err)
        .expect("an untyped restrict violation must still yield a constraint hint");
    assert!(
        hint.matches(
            "role_assignments_role_definition_id_fkey",
            &["role_definition_id"]
        ),
        "the hint must attribute the violation to the assignments FK so the repo \
         can refine it into RoleDefinitionAssignmentsExist"
    );
}

/// The counterpart: an untyped error that is NOT a constraint violation must
/// still yield no hint, so the fallback cannot manufacture an attribution out
/// of arbitrary driver text.
#[test]
fn extract_constraint_hint_ignores_untyped_non_constraint_errors() {
    let err = DbErr::Exec(RuntimeErr::Internal(
        "error returned from database: connection reset by peer".to_owned(),
    ));
    assert!(extract_constraint_hint(&err).is_none());
}

#[test]
fn dberror_other_routes_to_internal_with_redacted_diagnostic() {
    // Non-Sea, non-availability `DbError` variants land in `Internal`
    // via `From<DbError>`. The diagnostic comes from
    // `redacted_db_diagnostic` — DSN / config text MUST NOT leak.
    let lifted: DomainError =
        DbError::UnknownDsn("postgres://secret_user:secret_pass@host/db".into()).into();
    match lifted {
        DomainError::Internal { diagnostic, .. } => {
            assert!(
                !diagnostic.contains("secret_user"),
                "raw DSN leaked into diagnostic: {diagnostic}"
            );
            assert!(
                !diagnostic.contains("secret_pass"),
                "raw DSN leaked into diagnostic: {diagnostic}"
            );
            assert!(
                diagnostic.contains("redacted"),
                "diagnostic must come from redacted_db_diagnostic, got: {diagnostic}"
            );
        }
        other => panic!("expected Internal, got {other:?}"),
    }
}
