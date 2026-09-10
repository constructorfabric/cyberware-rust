//! What the database said about a refusal, structurally.
//!
//! Three ways to tell why a write was refused existed before this module, and
//! none of them was both complete and stable (issue #4645):
//!
//! * [`sea_orm::DbErr::sql_err`] is typed and portable, but its table is
//!   incomplete — `sea-orm` 2.0 maps `23505` and `23503` for `PostgreSQL` and
//!   has no `Check` discriminant at all, so a `PostgreSQL` 18 `RESTRICT`
//!   refusal (`23001`) returns `None`.
//! * Matching `DbErr`'s message text works until the driver rewords it or the
//!   server is asked to speak another language. `PostgreSQL` 18 did reword the
//!   `RESTRICT` message, which is the kind of change that path cannot survive.
//! * Reading the driver error directly is correct, and used to require a gear
//!   to depend on `sqlx` itself — which the architecture lints forbid, and
//!   which is why the classification ended up copied into gears.
//!
//! This module is the third one, without the dependency: the accessor returns
//! owned strings, so **no `sqlx` type appears in its signature** and a gear
//! that classifies never links the driver.
//!
//! # What belongs here and what does not
//!
//! A SQLSTATE names a *condition*: "a unique constraint was violated". What
//! that condition means for a caller — `409 Conflict`, a domain error, a retry
//! — is the gear's decision (`docs/arch/errors/ADR/0004`, which delegates
//! fine-grained mapping to gears). So this module classifies conditions and
//! stops there.

use sea_orm::DbErr;

/// The condition a SQLSTATE names, for the codes this platform acts on.
///
/// `#[non_exhaustive]`: the table grows as the platform starts acting on more
/// conditions, and that must not be a breaking change for a `match` in a gear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConstraintViolation {
    /// A unique index or constraint rejected the row.
    Unique,
    /// A foreign key rejected the row, or refused to leave one orphaned.
    ///
    /// Both `23503` (`foreign_key_violation`) and `23001`
    /// (`restrict_violation`) land here. They are genuinely different
    /// constraint actions — `RESTRICT` is checked immediately and cannot be
    /// deferred, `NO ACTION` can — and `PostgreSQL` 18 started reporting the
    /// first one under its own standard code. For a caller the condition is
    /// one and the same: rows still reference this one.
    ForeignKey,
    /// A `CHECK` constraint rejected the row.
    Check,
    /// A `NOT NULL` column was given no value.
    NotNull,
    /// An `EXCLUDE` constraint rejected the row.
    Exclusion,
}

/// The code the driver reported for a refusal, and the constraint name when it
/// named one.
///
/// Owned rather than borrowed so the type carries nothing from the driver: the
/// point of this module is that a gear can read a refusal without depending on
/// `sqlx`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverRefusal {
    code: String,
    constraint: Option<String>,
}

impl DriverRefusal {
    /// The code the driver reported, verbatim.
    ///
    /// On `PostgreSQL` and `MySQL` this is the five-character SQLSTATE. On
    /// `SQLite` it is the extended result code (`2067` for a unique
    /// violation), which is not a SQLSTATE and which
    /// [`constraint_violation`] therefore names no condition for — the
    /// accessor is called `code` rather than `sqlstate` for that reason.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The constraint the server named, when it named one.
    ///
    /// This is what tells two constraints on one table apart — which of two
    /// foreign keys refused, which unique index was hit — and no message-text
    /// classifier can do it reliably.
    #[must_use]
    pub fn constraint(&self) -> Option<&str> {
        self.constraint.as_deref()
    }

    /// The condition this code names, if it is one the table covers.
    #[must_use]
    pub fn violation(&self) -> Option<ConstraintViolation> {
        constraint_violation(&self.code)
    }
}

/// The condition a `PostgreSQL` SQLSTATE names.
///
/// Class 23 (`integrity_constraint_violation`) only, because that is the class
/// a caller can act on. Deliberate omissions:
///
/// * `23000` — `PostgreSQL` never sends it, and `MySQL` sends it for unique
///   *and* foreign-key violations alike, so it names no single condition. A
///   `MySQL` caller needs the vendor error number, which is what
///   [`sea_orm::DbErr::sql_err`] already reads.
/// * `SQLite` extended codes are not SQLSTATEs and are not in this table;
///   [`crate::secure::is_unique_violation`] and
///   [`crate::secure::is_foreign_key_violation`] remain the portable path.
#[must_use]
pub fn constraint_violation(sqlstate: &str) -> Option<ConstraintViolation> {
    match sqlstate {
        "23505" => Some(ConstraintViolation::Unique),
        // One condition, two codes. See `ConstraintViolation::ForeignKey`.
        "23503" | "23001" => Some(ConstraintViolation::ForeignKey),
        "23514" => Some(ConstraintViolation::Check),
        "23502" => Some(ConstraintViolation::NotNull),
        "23P01" => Some(ConstraintViolation::Exclusion),
        _ => None,
    }
}

/// The driver's code and constraint name inside a [`DbErr`], if the driver
/// reported a refusal.
///
/// `None` means the error carries no driver-level refusal: a connection
/// failure, an error `SeaORM` generated itself, or a build of this crate with
/// no database driver at all. It never means "the refusal had no code" for an
/// error that does carry one.
///
/// # Example
///
/// ```rust,ignore
/// use toolkit_db::db_error::{ConstraintViolation, driver_refusal};
///
/// match store.delete(id).await {
///     Err(err) => {
///         let refused_by_a_reference = driver_refusal(&err)
///             .and_then(|refusal| refusal.violation())
///             == Some(ConstraintViolation::ForeignKey);
///         // ... map that to this gear's own error
///     }
///     Ok(()) => {}
/// }
/// ```
#[must_use]
pub fn driver_refusal(err: &DbErr) -> Option<DriverRefusal> {
    // The body needs a driver; the signature does not, so callers never have to
    // gate on a backend feature to name this function. A build with no driver
    // cannot produce a driver error either, so `None` is the truth there rather
    // than a silent degradation.
    #[cfg(any(feature = "pg", feature = "mysql", feature = "sqlite"))]
    {
        // The same shape `DbErr::sql_err` reads, plus `Conn`: transient
        // connection codes (class 08, `57P01` and friends) arrive there, and a
        // caller deciding whether to retry wants them.
        let (DbErr::Exec(sea_orm::RuntimeErr::SqlxError(sqlx_err))
        | DbErr::Query(sea_orm::RuntimeErr::SqlxError(sqlx_err))
        | DbErr::Conn(sea_orm::RuntimeErr::SqlxError(sqlx_err))) = err
        else {
            return None;
        };
        let sqlx::Error::Database(db_err) = &**sqlx_err else {
            return None;
        };
        Some(DriverRefusal {
            code: db_err.code()?.into_owned(),
            constraint: db_err.constraint().map(ToOwned::to_owned),
        })
    }
    #[cfg(not(any(feature = "pg", feature = "mysql", feature = "sqlite")))]
    {
        let _ = err;
        None
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::{ConstraintViolation, constraint_violation, driver_refusal};

    /// The point of the table: one condition, both codes. A `RESTRICT` refusal
    /// on `PostgreSQL` 18 and a `NO ACTION` refusal on any major must classify
    /// the same, or a gear answers 500 on one of them.
    #[test]
    fn restrict_and_foreign_key_are_one_condition() {
        assert_eq!(
            constraint_violation("23001"),
            Some(ConstraintViolation::ForeignKey)
        );
        assert_eq!(
            constraint_violation("23503"),
            Some(ConstraintViolation::ForeignKey)
        );
    }

    #[test]
    fn the_other_class_23_codes_are_named() {
        assert_eq!(
            constraint_violation("23505"),
            Some(ConstraintViolation::Unique)
        );
        assert_eq!(
            constraint_violation("23514"),
            Some(ConstraintViolation::Check)
        );
        assert_eq!(
            constraint_violation("23502"),
            Some(ConstraintViolation::NotNull)
        );
        assert_eq!(
            constraint_violation("23P01"),
            Some(ConstraintViolation::Exclusion)
        );
    }

    /// `23000` is `MySQL`'s answer for unique *and* foreign-key violations, so
    /// naming a condition for it would be a guess. Unknown codes are `None`,
    /// which every caller reads as "classify this some other way".
    #[test]
    fn an_ambiguous_or_unknown_code_names_no_condition() {
        assert_eq!(constraint_violation("23000"), None);
        assert_eq!(constraint_violation("40001"), None);
        assert_eq!(constraint_violation(""), None);
    }

    /// A refusal from a real driver, reached end to end, with no server to
    /// start: `SQLite` in memory is enough to exercise the extraction path and
    /// all three accessors.
    ///
    /// It also pins the documented `SQLite` behaviour rather than assuming it:
    /// the driver reports an extended result code (`2067`), not a SQLSTATE, and
    /// names no constraint — so the table names no condition for it and a
    /// `SQLite` caller keeps using
    /// [`crate::secure::is_unique_violation`]. The `PostgreSQL` half, where the
    /// code *is* a SQLSTATE, is covered by
    /// `tests/error_classification.rs::pg_restrict_delete_is_classified_as_foreign_key_violation`
    /// against a real server.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn a_real_sqlite_refusal_is_reached_but_names_no_condition() {
        use sea_orm::ConnectionTrait as _;

        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        db.execute_unprepared("CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT UNIQUE)")
            .await
            .expect("create");
        db.execute_unprepared("INSERT INTO t (id, k) VALUES (1, 'a')")
            .await
            .expect("first insert");

        let err = db
            .execute_unprepared("INSERT INTO t (id, k) VALUES (2, 'a')")
            .await
            .expect_err("the unique index must refuse the second insert");

        let refusal = driver_refusal(&err).expect("a driver refusal must be reachable");
        assert_eq!(
            refusal.code(),
            "2067",
            "SQLITE_CONSTRAINT_UNIQUE, the extended result code"
        );
        assert_eq!(
            refusal.constraint(),
            None,
            "SQLite names no constraint in its error"
        );
        assert_eq!(
            refusal.violation(),
            None,
            "an extended result code is not a SQLSTATE, so the table names no \
             condition for it"
        );
        // The portable classifier still recognises it, which is the path a
        // SQLite caller is meant to use.
        assert!(crate::secure::is_unique_violation(&err));
    }

    /// An error `SeaORM` produced itself carries no driver refusal. Asserted
    /// because the accessor's contract is that `None` means "no driver error
    /// here", never "the code was missing".
    #[test]
    fn an_internal_error_carries_no_driver_refusal() {
        assert!(driver_refusal(&sea_orm::DbErr::Custom("not from a driver".into())).is_none());
        assert!(driver_refusal(&sea_orm::DbErr::RecordNotFound("nope".into())).is_none());
    }
}
