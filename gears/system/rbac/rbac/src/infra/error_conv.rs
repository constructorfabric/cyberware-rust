//! Infrastructure-layer DB error classification helpers used by the
//! boundary mapping in [`crate::infra::canonical_mapping`].
//!
//! These are the typed predicates the boundary mapping relies on:
//! backend-aware retryable-contention detection, connectivity signals,
//! and the redacted diagnostic helper that drops operator-supplied text
//! (DSN, env-var values) before logging.
//!
//! `is_unique_violation` is re-exported from
//! [`toolkit_db::secure`] instead of being re-implemented here —
//! `toolkit_db` is the canonical source of typed SQLSTATE classifiers.

use sea_orm::{DbBackend, DbErr};
use toolkit_db::DbError;
use toolkit_db::contention::is_retryable_contention;
use toolkit_db::secure::{ScopeError, is_unique_violation};

/// Backend-agnostic adapter — RBAC supports Postgres in production and
/// `SQLite` for tests and demos, and probes both because the boundary
/// classifier has no access to the live `DbBackend`.
///
/// # Trusted-discriminant gate
///
/// A serialization conflict (SQLSTATE 40001 / 40P01) is not representable by
/// `sea_orm::SqlErr`, so a real one always arrives unstructured
/// (`sql_err() == None`); a typed error is a unique/FK violation and never a
/// conflict. That distinction is load-bearing: the underlying
/// `is_retryable_contention` probes the error's `Display` for a bare `40001`
/// token, this function runs FIRST in `classify_db_err_to_domain`'s ladder,
/// and a constraint violation whose `DETAIL` merely embeds those digits — a
/// `principal_id`, a scope segment, a generated id — would otherwise be
/// misrouted to a retryable `Aborted` and retried forever against an
/// operation that can never succeed.
pub fn is_serialization_failure(err: &DbErr) -> bool {
    if err.sql_err().is_some() {
        return false;
    }
    // A known constraint violation is never a serialization conflict, so it
    // is classified out before the substring probe below can see it.
    if is_unique_violation(err) || is_check_violation(err) || is_fk_violation(err) {
        return false;
    }
    is_retryable_contention(DbBackend::Postgres, err)
        || is_retryable_contention(DbBackend::Sqlite, err)
}

/// Returns `true` iff `err` represents a `CHECK` constraint violation on
/// either supported backend.
///
/// String-based classification, because `SqlErr` has no `Check` discriminant.
/// Numeric SQLSTATEs are anchored inside code-shape tokens so unrelated
/// `DbErr` payloads carrying those digits in offsets, ports, or counters do
/// not match.
///
/// # Locale fragility
///
/// The English keyword fallbacks (`"check constraint"`, `"check_violation"`,
/// `"sqlite_constraint_check"`) only fire on hosts with the default English
/// `lc_messages`; on a localised PG cluster the SQLSTATE-anchored detectors
/// (`23514` for PG, `275` for `SQLite`) carry the classification alone. Those
/// are the trusted path — the keywords are best-effort for the rare
/// `RuntimeErr::Internal(_)` shape `SeaORM` left unstructured, so do not rely
/// on them for new code. A proper fix needs an upstream
/// `SqlErr::CheckConstraintViolation` discriminant in `sea_orm`.
pub fn is_check_violation(err: &DbErr) -> bool {
    // A real CHECK violation is not representable by `sea_orm::SqlErr`, so it
    // always arrives unstructured; a typed error is therefore never a CHECK.
    if err.sql_err().is_some() {
        return false;
    }
    let msg = err.to_string().to_lowercase();
    msg.contains("check constraint")
        || msg.contains("check_violation")
        || msg.contains("sqlite_constraint_check")
        || contains_anchored_pg_check_sqlstate(&msg)
        || (msg.contains("sqlite") && contains_anchored_sqlite_check_code(&msg))
}

fn contains_anchored_pg_check_sqlstate(msg: &str) -> bool {
    msg.contains("sqlstate 23514")
        || msg.contains("sqlstate: 23514")
        || msg.contains("sqlstate=23514")
        || msg.contains("code 23514")
        || msg.contains("code: 23514")
        || msg.contains("(23514)")
        || msg.contains("(23514:")
        || msg.starts_with("23514:")
        || msg.contains(" 23514:")
}

fn contains_anchored_sqlite_check_code(msg: &str) -> bool {
    msg.contains("code 275")
        || msg.contains("code: 275")
        || msg.contains("(275)")
        || msg.contains("(275:")
        || msg.starts_with("275:")
        || msg.contains(" 275:")
}

/// Returns `true` iff `err` represents a foreign-key constraint violation
/// (`SQLSTATE 23503` or `23001` on Postgres, `SQLITE_CONSTRAINT_FOREIGNKEY`
/// extended code on `SQLite`). Mirrors [`is_check_violation`]'s
/// dual-detection strategy: typed `SqlErr` fast path first, then a
/// lowercased substring fallback for proxies that strip the structured
/// discriminator.
///
/// # Two Postgres codes, one condition
///
/// `PostgreSQL` 18 reports an `ON DELETE RESTRICT` violation as the standard
/// `23001` (`restrict_violation`); 17 and earlier reported `23503`
/// (`foreign_key_violation`) for the same condition. Both mean "the row is
/// still referenced", so both classify here — matching the `23503 | 23001`
/// pairing in `timescaledb-usage-collector-plugin`'s storage classifier.
///
/// `sea_orm` types only `23503`, so a `23001` arrives untyped and is caught
/// by the substring fallback below rather than the fast path.
pub fn is_fk_violation(err: &DbErr) -> bool {
    // Trust the typed discriminant. A structured-but-different
    // `SqlErr` (e.g. a unique violation) is authoritative — do NOT
    // reclassify it as FK from free-form message text, which can embed
    // caller-supplied values. Only fall back to keyword matching for the
    // unstructured shape sea_orm could not type (`sql_err() == None`).
    match err.sql_err() {
        Some(sea_orm::SqlErr::ForeignKeyConstraintViolation(_)) => true,
        Some(_) => false,
        None => {
            let msg = err.to_string().to_lowercase();
            msg.contains("foreign key constraint")
                || msg.contains("foreign_key_violation")
                || msg.contains("restrict_violation")
                || msg.contains("sqlstate 23503")
                || msg.contains("sqlstate: 23503")
                || msg.contains("sqlstate=23503")
                || msg.contains("(23503)")
                || msg.contains("(23503:")
                || msg.contains("sqlstate 23001")
                || msg.contains("sqlstate: 23001")
                || msg.contains("sqlstate=23001")
                || msg.contains("(23001)")
                || msg.contains("(23001:")
        }
    }
}

/// Returns `true` iff `err` is a typed database connectivity / outage
/// signal — pool acquire timeout, connection closed, connection-level
/// runtime error, or a raw `std::io::Error` surfaced through
/// [`DbError::Io`]. Used to route those failures to HTTP 503 rather than
/// HTTP 500.
///
/// Classification is deliberately conservative: only typed signals from
/// `sea_orm::DbErr` and the `toolkit_db` wrapper count. Unstructured
/// `RuntimeErr::Internal(String)` text stays in the `Internal` bucket.
pub fn is_db_availability_error(err: &DbError) -> bool {
    matches!(
        err,
        DbError::Io(_) | DbError::Sea(DbErr::ConnectionAcquire(_) | DbErr::Conn(_))
    )
}

/// Returns a non-secret string description of `err` suitable for the
/// `rbac.db` `warn!` log and for the `Internal::diagnostic` audit field.
///
/// Config-bearing variants (`UnknownDsn`, `InvalidConfig`,
/// `ConfigConflict`, `InvalidSqlitePragma`, `UnknownSqlitePragma`,
/// `InvalidParameter`, `SqlitePragma`, `EnvVar`, `UrlParse`) can carry
/// DSN strings, env-var names/values, or other operator-supplied text
/// that may include passwords / hostnames / tokens — their bodies are
/// dropped, only the variant kind survives. Pass-through wrappers
/// (`Sqlx`, `Sea`, `Io`, `Lock`, `Other`) are also reduced to a kind
/// label because their `Display` impls forward arbitrary driver text.
pub fn redacted_db_diagnostic(err: &DbError) -> &'static str {
    match err {
        DbError::UnknownDsn(_) => "db error: unknown DSN (text redacted)",
        DbError::FeatureDisabled(_) => "db error: feature not enabled",
        DbError::InvalidConfig(_) => "db error: invalid configuration (text redacted)",
        DbError::ConfigConflict(_) => "db error: configuration conflict (text redacted)",
        DbError::InvalidSqlitePragma { .. } => {
            "db error: invalid SQLite pragma parameter (text redacted)"
        }
        DbError::UnknownSqlitePragma(_) => "db error: unknown SQLite pragma (text redacted)",
        DbError::InvalidParameter(_) => "db error: invalid connection parameter (text redacted)",
        DbError::SqlitePragma(_) => "db error: SQLite pragma error (text redacted)",
        DbError::EnvVar { .. } => "db error: environment variable error (text redacted)",
        DbError::UrlParse(_) => "db error: URL parse error (text redacted)",
        DbError::Sqlx(_) => "db error: sqlx (text redacted)",
        DbError::Sea(_) => "db error: sea-orm (text redacted)",
        DbError::Io(_) => "db error: io (text redacted)",
        DbError::Lock(_) => "db error: lock (text redacted)",
        DbError::Other(_) => "db error: other (text redacted)",
        DbError::ConnRequestedInsideTx => {
            "db error: connection requested inside active transaction"
        }
    }
}

/// Non-secret description of a [`ScopeError`] for any log line or error
/// envelope that would otherwise interpolate `ScopeError`'s `Display`.
///
/// `ScopeError::Db(DbErr)`'s `Display` forwards arbitrary driver text (DSN,
/// host, SQL fragments, PG `DETAIL: Key (col)=(value)` echoing caller-supplied
/// values) — that body must never reach the `rbac.db`, startup, or bootstrap
/// log streams. Used by the seeder and bootstrap envelopes, and by the repo
/// paths under the same invariant (e.g. `fetch_current_etag` in
/// `role_definition_repo`). The remaining variants (`Invalid`,
/// `TenantNotInScope`, `Denied`) carry only static labels or a tenant id, so
/// their `Display` is safe to surface verbatim.
pub fn redacted_scope_error(err: &ScopeError) -> String {
    match err {
        ScopeError::Db(_) => "scope error: database error (driver text redacted)".to_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::RuntimeErr;

    /// Build an unstructured `DbErr` (no typed `SqlErr`) carrying `msg`,
    /// matching the shape a proxy / runtime-internal error takes — the only
    /// shape the keyword/SQLSTATE fallbacks are meant to classify.
    fn runtime(msg: &str) -> DbErr {
        DbErr::Query(RuntimeErr::Internal(msg.to_owned()))
    }

    #[test]
    fn serialization_failure_detects_pg_40001_when_unstructured() {
        assert!(is_serialization_failure(&runtime(
            "could not serialize access due to read/write dependencies (SQLSTATE 40001)"
        )));
    }

    #[test]
    fn serialization_failure_false_for_known_constraint_violation() {
        // A CHECK-violation message is NOT a serialization conflict, even
        // unstructured — guards the "infinite retry of a never-succeeding op".
        assert!(!is_serialization_failure(&runtime(
            "new row violates check constraint \"ck_x\""
        )));
    }

    #[test]
    fn serialization_failure_false_for_plain_error() {
        assert!(!is_serialization_failure(&runtime(
            "some unrelated failure"
        )));
    }

    #[test]
    fn check_violation_matches_keyword_and_sqlstate_anchors() {
        assert!(is_check_violation(&runtime(
            "new row violates check constraint \"ck\""
        )));
        assert!(is_check_violation(&runtime(
            "ERROR something (23514) detail"
        )));
        assert!(is_check_violation(&runtime(
            "sqlite failure (275) on insert"
        )));
        assert!(!is_check_violation(&runtime("plain error code 12345")));
    }

    #[test]
    fn fk_violation_matches_keyword_fallback() {
        assert!(is_fk_violation(&runtime(
            "insert violates foreign key constraint \"fk_role\""
        )));
        assert!(is_fk_violation(&runtime("failure sqlstate 23503 detail")));
        assert!(!is_fk_violation(&runtime("not a constraint problem")));
    }

    #[test]
    fn db_availability_error_only_for_io_and_conn() {
        let io = DbError::Io(std::io::Error::other("conn reset"));
        assert!(is_db_availability_error(&io));
        let sea_other = DbError::Sea(DbErr::Custom("boom".to_owned()));
        assert!(!is_db_availability_error(&sea_other));
    }

    #[test]
    fn redacted_db_diagnostic_drops_operator_text() {
        let io = DbError::Io(std::io::Error::other("secret-host:5432 refused"));
        let red = redacted_db_diagnostic(&io);
        assert_eq!(red, "db error: io (text redacted)");
        assert!(!red.contains("secret-host"));

        let sea = DbError::Sea(DbErr::Custom("dsn=postgres://u:pw@h/db".to_owned()));
        assert!(!redacted_db_diagnostic(&sea).contains("pw"));

        let other = DbError::Other(anyhow::anyhow!("token=abc123"));
        assert!(!redacted_db_diagnostic(&other).contains("abc123"));
    }

    #[test]
    fn redacted_scope_error_redacts_db_but_forwards_others() {
        let db = ScopeError::Db(DbErr::Custom("dsn secret".to_owned()));
        let red = redacted_scope_error(&db);
        assert!(!red.contains("secret"), "DB driver text MUST be redacted");
        assert!(red.contains("database error"));

        // Non-DB variants carry only static labels — forwarded verbatim.
        let denied = ScopeError::Denied("not accessible");
        assert_eq!(redacted_scope_error(&denied), denied.to_string());
    }
}
