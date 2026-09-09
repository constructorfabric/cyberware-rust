use uuid::Uuid;

/// Errors that can occur during scoped query execution.
///
/// `#[non_exhaustive]`: this is the error enum every gear's repository layer
/// matches on, and the ORM grows variants a gear has no specific answer for
/// (the graph-query refusals below are constructible only under the `pgq`
/// feature). A downstream `match` keeps one wildcard arm for those instead of
/// gaining a dead arm per new variant — the cost is that a future variant a
/// gear *should* handle specifically lands in the wildcard rather than failing
/// its build, which is the standard trade for a library error enum.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum ScopeError {
    /// Database error occurred during query execution.
    #[error("database error: {0}")]
    Db(#[from] sea_orm::DbErr),

    /// Invalid scope configuration.
    #[error("invalid scope: {0}")]
    Invalid(&'static str),

    /// Tenant isolation violation: `tenant_id` is not included in the current scope.
    #[error("access denied: tenant_id not present in security scope ({tenant_id})")]
    TenantNotInScope { tenant_id: Uuid },

    /// Operation denied - entity not accessible in current security scope.
    #[error("access denied: {0}")]
    Denied(&'static str),

    /// A graph pattern element on which no constraint of the live scope
    /// resolves. Compiling it would produce a deny-all traversal that reads as
    /// missing data, so it is refused by name instead
    /// (`docs/arch/secure-orm/ADR/0002`, Policy 2).
    #[error(
        "invalid scope: no constraint of the scope resolves on graph element \
         `{element}` (property `{property}` does not resolve on its entity)"
    )]
    UnresolvedScopeProperty {
        /// The pattern variable of the refused element.
        element: &'static str,
        /// A property the scope addresses that the element's entity cannot map.
        property: String,
    },

    /// The SQL/PGQ syntax layer refused to render a graph declaration or
    /// pattern: no projected columns, a duplicate pattern variable, an empty
    /// identifier, an endpoint whose key and referenced columns differ in
    /// arity. The message is the syntax layer's own.
    ///
    /// The payload is this crate's, not the syntax crate's error type. The
    /// variant exists in every build — a feature-gated variant would change
    /// the enum's shape under feature unification — while the syntax crate is
    /// linked only under `pgq`: a gear on `SQLite` must not carry a `PostgreSQL` 19
    /// dependency to name this arm (`docs/arch/secure-orm/ADR/0002`, "Backend
    /// gating").
    #[error("graph syntax error: {0}")]
    GraphSyntax(String),
}

impl ScopeError {
    /// Returns `true` if this error wraps a unique-constraint violation.
    #[must_use]
    pub fn is_unique_violation(&self) -> bool {
        match self {
            Self::Db(db_err) => is_unique_violation(db_err),
            _ => false,
        }
    }

    /// Returns `true` if this error wraps a foreign-key violation.
    #[must_use]
    pub fn is_foreign_key_violation(&self) -> bool {
        match self {
            Self::Db(db_err) => is_foreign_key_violation(db_err),
            _ => false,
        }
    }
}

/// Check whether a `sea_orm::DbErr` represents a unique-constraint violation.
///
/// First tries `SeaORM`'s built-in `sql_err()` detection (SQLSTATE-based).
/// Falls back to string matching on the error message for cases where
/// `sql_err()` fails to classify the error (e.g. certain connection proxies
/// or driver wrappers that strip the SQLSTATE code).
///
/// Recognized patterns across backends:
/// - **Postgres** SQLSTATE `23505` — "`unique_violation`" / "duplicate key"
/// - **`SQLite`** extended code `2067` — "UNIQUE constraint failed"
/// - **`MySQL`** error `1062` — "Duplicate entry"
#[must_use]
pub fn is_unique_violation(err: &sea_orm::DbErr) -> bool {
    // Fast path: SeaORM parsed the SQLSTATE / vendor code correctly.
    if matches!(
        err.sql_err(),
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
    ) {
        return true;
    }

    // Fallback: string-based detection for wrapped / proxied errors.
    let msg = err.to_string().to_lowercase();
    msg.contains("unique constraint")
        || msg.contains("duplicate key")
        || msg.contains("unique_violation")
        || msg.contains("duplicate entry")
        || msg.contains("unique constraint failed")
}

/// Check whether a `sea_orm::DbErr` represents a foreign-key violation.
///
/// The counterpart of [`is_unique_violation`], and detected the same way: the
/// SQLSTATE fast path first, then a message match for errors that were
/// re-wrapped on the way here and lost their typed shape.
///
/// Useful where a referencing row is the invariant and the `RESTRICT` on the
/// foreign key is what actually enforces it -- a preceding count is a nicer
/// message, not the guard, and under concurrency the constraint is what
/// answers.
///
/// Recognized patterns across backends:
/// - **Postgres** SQLSTATE `23503` — "`foreign_key_violation`" / "violates
///   foreign key constraint"
/// - **`SQLite`** extended code `787` (`SQLITE_CONSTRAINT_FOREIGNKEY`) —
///   "FOREIGN KEY constraint failed"
/// - **`MySQL`** errors `1451`/`1452` — "a foreign key constraint fails"
#[must_use]
pub fn is_foreign_key_violation(err: &sea_orm::DbErr) -> bool {
    if matches!(
        err.sql_err(),
        Some(sea_orm::SqlErr::ForeignKeyConstraintViolation(_))
    ) {
        return true;
    }

    let msg = err.to_string().to_lowercase();
    msg.contains("foreign key constraint")
        || msg.contains("foreign_key_violation")
        || msg.contains("violates foreign key")
}

#[cfg(test)]
mod tests {
    use super::{is_foreign_key_violation, is_unique_violation};
    use sea_orm::DbErr;

    // The classifiers are reached through two shapes: the typed `SqlErr` the
    // driver produces, and the `DbErr::Custom` left by a caller that
    // re-wrapped the error through `to_string()`.
    //
    // In this workspace the *typed* shape is the production one for these two
    // functions: every call site classifies the raw `ScopeError::Db` straight
    // out of `secure_insert`/`secure_delete`, and only stringifies what the
    // classifier already rejected. (`is_retryable_contention` is the opposite
    // case, and the one RG-15 was about -- do not carry that conclusion
    // across.)
    //
    // The typed path cannot be exercised from here: it needs a `DbErr` whose
    // `sql_err()` resolves, and that requires a real `PgDatabaseError` or
    // `SqliteError`, both of which have crate-private constructors. It is
    // covered end-to-end instead, by tests that provoke a genuine violation
    // against live SQLite. What is left for a unit test is the message
    // matching below, per backend.

    #[test]
    fn foreign_key_violation_detected_per_backend_message() {
        for msg in [
            "error returned from database: update or delete on table \"gts_type\" violates \
             foreign key constraint \"resource_group_gts_type_id_fkey\" on table \
             \"resource_group\"",
            "error returned from database: (code: 787) FOREIGN KEY constraint failed",
            "Cannot delete or update a parent row: a foreign key constraint fails",
        ] {
            assert!(
                is_foreign_key_violation(&DbErr::Custom(msg.to_owned())),
                "should classify as a foreign-key violation: {msg}"
            );
        }
    }

    #[test]
    fn foreign_key_and_unique_are_not_confused() {
        // They map to different domain answers -- "still referenced" versus
        // "already exists" -- so a classifier that matched both would report
        // the wrong conflict.
        let unique = DbErr::Custom("UNIQUE constraint failed: gts_type.schema_id".to_owned());
        let unique_pg = DbErr::Custom(
            "error returned from database: duplicate key value violates unique constraint \
             \"gts_type_schema_id_key\""
                .to_owned(),
        );
        assert!(is_unique_violation(&unique_pg));
        assert!(!is_foreign_key_violation(&unique_pg));
        let fk = DbErr::Custom("FOREIGN KEY constraint failed".to_owned());

        assert!(is_unique_violation(&unique));
        assert!(!is_foreign_key_violation(&unique));

        assert!(is_foreign_key_violation(&fk));
        assert!(!is_unique_violation(&fk));
    }

    #[test]
    fn an_unrelated_error_is_neither() {
        let err = DbErr::Custom("connection reset by peer".to_owned());
        assert!(!is_unique_violation(&err));
        assert!(!is_foreign_key_violation(&err));
    }
}
