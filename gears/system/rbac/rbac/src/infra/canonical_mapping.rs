//! DB-error → [`DomainError`] classification ladder.
//!
//! Lives in `infra/` because the classifier reads `sea_orm::DbErr`
//! SQLSTATE codes and `toolkit_db::DbError` variant discriminants —
//! both forbidden inside `domain/` by the layering rules. Keeping the
//! classifier here lets [`crate::domain::error::DomainError`] stay pure
//! (no `sea_orm` / `toolkit_db` imports) while still routing DB failures
//! onto the AIP-193 canonical categories.
//!
//! # Lift vs classify
//!
//! Two entry points convert raw DB errors into [`DomainError`]:
//!
//! - [`classify_db_err_to_domain`] — the **sole** `DbErr → DomainError`
//!   path. Returns generic AIP-193 variants (`AlreadyExists`,
//!   `Conflict`, `Validation`, `Aborted`, `ServiceUnavailable`,
//!   `Internal`); does not know any RBAC-specific constraint names.
//! - [`From<DbError> for DomainError`] — non-transactional code paths
//!   (`repo.db.conn()?`); routes `Sea` variants through the classifier,
//!   `Io` outages to `ServiceUnavailable`, everything else to
//!   `Internal` with a redacted diagnostic.
//!
//! # Constraint-hint refinement
//!
//! Repos that want to refine a generic `AlreadyExists` / `Conflict` into
//! a typed variant (`RoleDefinitionNameTaken { name, owner_tenant_id }`,
//! `RoleAssignmentDuplicate { … }`, …) call [`extract_constraint_hint`]
//! on the same `DbErr` **before** handing it to the classifier. The
//! returned [`ConstraintHint`] exposes structured constraint-identity
//! checks (`hint.matches("uq_role_name_per_tenant", &["name", "owner_tenant_id"])`)
//! without leaking `sea_orm::SqlErr` / sqlx machinery into the repo
//! code. This keeps "every `DbErr` is decoded in one place" while still
//! letting the SDK boundary carry typed conflict variants.

use sea_orm::{DbErr, RuntimeErr, SqlErr};
use toolkit_db::DbError;
use toolkit_db::secure::is_unique_violation;
use tracing::warn;

use crate::domain::error::DomainError;
use crate::infra::error_conv::{
    is_check_violation, is_db_availability_error, is_fk_violation, is_serialization_failure,
    redacted_db_diagnostic,
};

/// Classify a raw [`DbErr`] into a typed [`DomainError`].
///
/// Ladder (mirrors the AIP-193 mapping the boundary applies, expressed
/// as typed `DomainError` variants so domain code stays free of
/// `sea_orm` references):
///
/// - SQLSTATE `40001` post-retry → [`DomainError::Aborted`] with
///   `reason = "SERIALIZATION_CONFLICT"`.
/// - Unique violation (`23505` / `SQLite` `2067`) →
///   [`DomainError::AlreadyExists`] (generic). Repos refine the
///   generic variant into typed RBAC-specific variants via
///   [`extract_constraint_hint`].
/// - Check violation (`23514` / `SQLite` `275`) →
///   [`DomainError::Validation`]. DB-side `CHECK` predicates are the
///   last line of defence behind the service-layer validators; routing
///   them to `Validation` keeps the public envelope at HTTP 400.
/// - FK violation (`23503` / `SQLite` `787`) →
///   [`DomainError::Conflict`] (generic). Repos refine via
///   [`extract_constraint_hint`] when the violated FK identifies a
///   typed RBAC variant (e.g. `RoleDefinitionAssignmentsExist`).
/// - Typed availability signal (pool timeout, transport drop) →
///   [`DomainError::ServiceUnavailable`].
/// - Anything else → [`DomainError::Internal`] with a
///   [`redacted_db_diagnostic`] string (no DSN / driver text leaks).
#[allow(
    clippy::cognitive_complexity,
    reason = "flat classification ladder; branchy warn! paths only, no logic"
)]
pub fn classify_db_err_to_domain(db_err: DbErr) -> DomainError {
    if is_serialization_failure(&db_err) {
        // Do NOT log the raw `DbErr` Display — its `DETAIL` can carry
        // caller-supplied values. The classification lives in the message.
        warn!(
            target: "rbac.db",
            "serialization failure observed mapped to DomainError::Aborted"
        );
        // This is the eager, non-transactional classifier — no retry
        // helper runs in front of it, so the detail states the conflict
        // was *observed* rather than claiming a retry budget was spent
        // (which would mislead operators on a first-attempt conflict).
        return DomainError::Aborted {
            reason: "SERIALIZATION_CONFLICT".to_owned(),
            detail: "serialization conflict observed; the operation may succeed if retried"
                .to_owned(),
        };
    }
    if is_unique_violation(&db_err) {
        // Log the structured constraint *name* (a non-secret schema
        // identifier), never the raw `DbErr` Display whose `DETAIL`
        // echoes caller-supplied key values.
        warn!(
            target: "rbac.db",
            constraint = structured_constraint_name(&db_err).unwrap_or("<unstructured>"),
            "unique-constraint violation mapped to DomainError::AlreadyExists"
        );
        return DomainError::AlreadyExists {
            detail: "request conflicts with existing state".to_owned(),
        };
    }
    if is_check_violation(&db_err) {
        // Structured constraint name only (no raw Display / DETAIL).
        warn!(
            target: "rbac.db",
            constraint = structured_constraint_name(&db_err).unwrap_or("<unstructured>"),
            "check-constraint violation mapped to DomainError::Validation"
        );
        return DomainError::Validation {
            detail: "request violates a server-side validation constraint".to_owned(),
        };
    }
    if is_fk_violation(&db_err) {
        // Unattributed FK violation — the repo-level mapper claims
        // RBAC-specific FKs (e.g. `role_assignments_role_definition_id_fkey`
        // → `RoleDefinitionAssignmentsExist`/`RoleDefinitionMissing`)
        // before delegating, so a violation surfacing here is a generic
        // referential-integrity failure with no typed RBAC variant.
        // 409 (Conflict) is the AIP-193 surface — not 500 — because the
        // caller can act on it by inspecting the conflicting row, while
        // the operator-meaningful detail (which FK, which constraint
        // name) goes to the `rbac.db` log only.
        // Structured constraint name only (no raw Display / DETAIL).
        warn!(
            target: "rbac.db",
            constraint = structured_constraint_name(&db_err).unwrap_or("<unstructured>"),
            "foreign-key violation mapped to DomainError::Conflict"
        );
        return DomainError::Conflict {
            detail: "referential integrity constraint violated".to_owned(),
        };
    }
    let wrapped = DbError::Sea(db_err);
    if is_db_availability_error(&wrapped) {
        warn!(
            target: "rbac.db",
            diagnostic = redacted_db_diagnostic(&wrapped),
            "DB availability failure mapped to DomainError::ServiceUnavailable"
        );
        return DomainError::ServiceUnavailable {
            detail: redacted_db_diagnostic(&wrapped).to_owned(),
            retry_after: None,
            cause: None,
        };
    }
    let redacted = redacted_db_diagnostic(&wrapped);
    warn!(
        target: "rbac.db",
        diagnostic = redacted,
        "unclassified DB error mapped to DomainError::Internal"
    );
    DomainError::Internal {
        diagnostic: format!("unclassified database error: {redacted}"),
        cause: None,
    }
}

// ---------------------------------------------------------------------
// Constraint-hint refinement API.
// ---------------------------------------------------------------------

/// Read-only metadata extracted from a [`DbErr`] for callers that want
/// to refine a generic [`DomainError::AlreadyExists`] /
/// [`DomainError::Conflict`] into a typed RBAC variant.
///
/// The hint exposes constraint identity (Postgres structured name +
/// `SQLite` column-set fallback) and lets the caller extract the
/// conflicting value from the driver-supplied detail string. It does
/// **not** participate in classification — [`classify_db_err_to_domain`]
/// is still the sole decider of which AIP-193 variant a `DbErr` maps to.
///
/// Repos use the hint exactly like this:
///
/// ```ignore
/// let hint = extract_constraint_hint(&err);
/// let mut domain = classify_db_err_to_domain(err);
/// if matches!(domain, DomainError::AlreadyExists { .. })
///     && let Some(h) = &hint
///     && h.matches("uq_role_name_per_tenant", &["name", "owner_tenant_id"])
/// {
///     domain = DomainError::RoleDefinitionNameTaken {
///         name: h.extract_quoted_value().unwrap_or_default(),
///         owner_tenant_id: None,
///     };
/// }
/// ```
#[derive(Debug)]
pub struct ConstraintHint {
    /// Postgres-only: the structured `constraint` field from sqlx's
    /// `DatabaseError`. `None` when sqlx stripped the field (older
    /// locales) or when running on `SQLite` (no concept of constraint
    /// names at the driver level).
    structured_name: Option<String>,
    /// Raw driver-supplied message body. Lowercased substring matching
    /// is the `SQLite` fallback when `structured_name` is `None`; the
    /// PG `Key (col, …)=(val, …)` detail extractor also reads from
    /// here.
    raw_message: String,
}

impl ConstraintHint {
    /// Returns `true` when the hint corresponds to the named constraint
    /// across both backends.
    ///
    /// Resolution order:
    /// 1. **Postgres fast path** — `structured_name == Some(pg_name)`.
    /// 2. **Postgres message fallback** — sqlx may strip the structured
    ///    field on non-English `lc_messages`; the formatted message
    ///    still embeds the constraint name as text in the default
    ///    English locale.
    /// 3. **`SQLite` column-set fallback** — `SQLite` errors look like
    ///    `UNIQUE constraint failed: table.col1, table.col2` and carry
    ///    no constraint name. The matcher succeeds when every column in
    ///    `sqlite_columns` appears in the message.
    ///
    /// An empty `sqlite_columns` slice means "any leftover violation
    /// matches" — safe only when the caller has already checked the
    /// more-specific constraints (vacuous truth of `Iterator::all`).
    pub(crate) fn matches(&self, pg_name: &str, sqlite_columns: &[&str]) -> bool {
        if let Some(ref name) = self.structured_name {
            return name == pg_name;
        }
        if self.raw_message.contains(pg_name) {
            return true;
        }
        sqlite_columns
            .iter()
            .all(|col| self.raw_message.contains(col))
    }

    /// Postgres-only: extract the quoted value from a
    /// `Key (col, …)=(val, …) already exists.` detail string. Returns `None`
    /// when the message lacks that segment (`SQLite`, or a PG locale that
    /// re-words the detail).
    ///
    /// RBAC uses this to recover the conflicting role name from a
    /// `uq_role_name_per_tenant` violation when building the typed
    /// `RoleDefinitionNameTaken { name }` variant.
    ///
    /// The parser splits on the first `,` or `)` after `)=(`, so a role name
    /// containing either character would arrive truncated — the domain
    /// boundary (`validate_name_charset` in `role_definition/service.rs`)
    /// rejects those on create/update, which bounds the input here. PG's
    /// DETAIL format does not double single quotes inside the value, so
    /// `O'Brien` arrives verbatim and needs no unescaping; a name containing a
    /// literal `''` round-trips unchanged.
    pub(crate) fn extract_quoted_value(&self) -> Option<String> {
        let after_equals = self.raw_message.split_once(")=(")?.1;
        let (value, _rest) = after_equals
            .split_once(',')
            .or_else(|| after_equals.split_once(')'))?;
        Some(value.to_owned())
    }
}

/// Extract a [`ConstraintHint`] from a [`DbErr`], for the shapes where
/// constraint refinement is meaningful: unique and foreign-key violations.
///
/// Takes `&DbErr` so the caller can still hand the owned `DbErr` to
/// [`classify_db_err_to_domain`] afterwards — there is no need to
/// clone the error.
///
/// # Why the untyped fallback exists
///
/// `sea_orm`'s `SqlErr` types SQLSTATE `23503` (`foreign_key_violation`)
/// and nothing else in that family. `PostgreSQL` 18 changed what an
/// `ON DELETE RESTRICT` violation reports: it is now the standard `23001`
/// (`restrict_violation`), where 17 and earlier reported `23503`. That
/// arrives as an untyped `DbErr` (`sql_err() == None`), so the typed fast
/// path alone silently stopped refining — a role definition with live
/// assignments degraded from `RoleDefinitionAssignmentsExist` (a typed 409
/// naming the cause) to a bare referential-integrity `Conflict`.
///
/// [`is_fk_violation`] already recognises both codes, so the fallback asks
/// it and then recovers the constraint name from sqlx directly. The same
/// `23503 | 23001` pairing is applied in
/// `timescaledb-usage-collector-plugin`'s storage error classifier.
pub fn extract_constraint_hint(err: &DbErr) -> Option<ConstraintHint> {
    let raw_message = match err.sql_err() {
        Some(
            SqlErr::UniqueConstraintViolation(raw) | SqlErr::ForeignKeyConstraintViolation(raw),
        ) => raw,
        // Everything else is refinable only if it is an untyped FK violation.
        // One arm covers both remaining cases because `is_fk_violation`
        // rejects a typed-but-different `SqlErr` outright — a typed
        // discriminant is authoritative and is never second-guessed from
        // message text.
        _ if is_fk_violation(err) => err.to_string(),
        _ => return None,
    };
    Some(ConstraintHint {
        structured_name: structured_constraint_name(err).map(str::to_owned),
        raw_message,
    })
}

/// Pull the structured `constraint` field out of sqlx's `DatabaseError`.
/// Returns `None` for any `DbErr` shape that does not carry a
/// `sqlx::Error::Database(_)`; the caller must fall back to message
/// matching. Reached via `SeaORM`'s `sea_orm::sqlx` re-export so this
/// crate needs no direct `sqlx` dep.
fn structured_constraint_name(err: &DbErr) -> Option<&str> {
    let (DbErr::Exec(runtime) | DbErr::Query(runtime)) = err else {
        return None;
    };
    let RuntimeErr::SqlxError(sqlx_err) = runtime else {
        return None;
    };
    // SeaORM 2 carries the sqlx error behind an `Arc` in `RuntimeErr::SqlxError`, so the
    // pattern match has to go through the smart pointer to reach the enum.
    if let sea_orm::sqlx::Error::Database(db) = &**sqlx_err {
        db.constraint()
    } else {
        None
    }
}

#[cfg(test)]
#[path = "canonical_mapping_tests.rs"]
mod canonical_mapping_tests;

impl From<DbError> for DomainError {
    /// Lift a [`DbError`] into the appropriate domain-internal variant.
    ///
    /// Routing:
    ///
    /// * `DbError::Sea(_)` → [`classify_db_err_to_domain`]. Non-transactional
    ///   paths (`repo.db.conn()?`) classify eagerly because there is no
    ///   retry helper to consult the raw `DbErr`.
    /// * Non-Sea variants that satisfy [`is_db_availability_error`]
    ///   (currently `DbError::Io(_)`) → [`DomainError::ServiceUnavailable`]
    ///   directly. They don't carry a `DbErr` for retry to inspect,
    ///   but they signal a transient infra outage that must surface
    ///   as HTTP 503, not HTTP 500.
    /// * Everything else → [`DomainError::Internal`] with a redacted
    ///   diagnostic. The raw error is preserved on the `cause` chain
    ///   for the audit trail, but the user-visible diagnostic carries
    ///   only the variant kind so DSN / env-var / driver text cannot
    ///   leak.
    fn from(err: DbError) -> Self {
        match err {
            DbError::Sea(db) => classify_db_err_to_domain(db),
            other if is_db_availability_error(&other) => Self::ServiceUnavailable {
                detail: redacted_db_diagnostic(&other).to_owned(),
                retry_after: None,
                cause: Some(std::sync::Arc::new(other)),
            },
            other => Self::Internal {
                diagnostic: redacted_db_diagnostic(&other).to_owned(),
                cause: Some(std::sync::Arc::new(other)),
            },
        }
    }
}
