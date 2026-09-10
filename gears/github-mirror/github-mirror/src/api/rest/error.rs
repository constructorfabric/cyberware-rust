//! How a `DomainError` reaches the caller: the HTTP-facing mapping lives in
//! the API layer, so `domain/` stays free of status codes and transport
//! vocabulary.

#[cfg(test)]
use toolkit_canonical_errors::Problem;
use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::DomainError;
use crate::redact::redacted;

#[resource_error(gts_id!("cf.core.github_mirror.repository.v1~"))]
pub struct RepositoryError;

/// What a caller is told about an internal failure. The cause is logged, not
/// returned: the messages name upstream GitHub paths and storage internals.
const INTERNAL_DETAIL: &str = "The mirror could not complete this request";

/// Which kind of database failure happened, without its text.
///
/// A `DbError`'s own message can carry a DSN, a server-returned value or the
/// statement that failed, so the log records the classification instead: it
/// is what an operator alerts on, and it cannot leak a credential into a log
/// sink.
const fn db_error_kind(e: &toolkit_db::DbError) -> &'static str {
    match e {
        toolkit_db::DbError::UnknownDsn(_)
        | toolkit_db::DbError::InvalidConfig(_)
        | toolkit_db::DbError::ConfigConflict(_)
        | toolkit_db::DbError::InvalidParameter(_)
        | toolkit_db::DbError::EnvVar { .. }
        | toolkit_db::DbError::UrlParse(_) => "configuration",
        toolkit_db::DbError::FeatureDisabled(_) => "feature_disabled",
        toolkit_db::DbError::InvalidSqlitePragma { .. }
        | toolkit_db::DbError::UnknownSqlitePragma(_)
        | toolkit_db::DbError::SqlitePragma(_) => "sqlite_pragma",
        toolkit_db::DbError::Sea(_) => "query",
        toolkit_db::DbError::Io(_) => "io",
        toolkit_db::DbError::Lock(_) => "advisory_lock",
        _ => "other",
    }
}

impl From<DomainError> for CanonicalError {
    // Flat match on the domain enum is the whole point of this conversion;
    // the structured `tracing::*!` macros count toward cognitive complexity
    // but splitting the arms into helpers would just hide the mapping.
    #[allow(clippy::cognitive_complexity)]
    fn from(e: DomainError) -> Self {
        match e {
            DomainError::NotFound => RepositoryError::not_found("Repo not found")
                .with_resource("repository")
                .create(),
            DomainError::Validation { field, message } => RepositoryError::invalid_argument()
                .with_field_violation(field, message, "VALIDATION_ERROR")
                .create(),
            DomainError::AccessLost(msg) => {
                tracing::warn!(msg = %redacted(&msg), "github-mirror upstream access lost");
                RepositoryError::not_found("Repo not found or not accessible")
                    .with_resource("repository")
                    .create()
            }
            DomainError::Conflict(msg) => RepositoryError::already_exists(msg)
                .with_resource("repository")
                .create(),
            DomainError::Forbidden(msg) => {
                tracing::warn!(msg = %redacted(&msg), "github-mirror access forbidden");
                RepositoryError::not_found("Repo not found or not accessible")
                    .with_resource("repository")
                    .create()
            }
            // Both arms keep the detail in the log and hand the caller a
            // fixed message: an internal failure's text names upstream
            // GitHub paths and storage internals, and a repository that is
            // private to one tenant should not be inferable from another
            // tenant's error body.
            DomainError::Internal(msg) => {
                tracing::error!(msg = %redacted(&msg), "github-mirror internal error");
                CanonicalError::internal(INTERNAL_DETAIL).create()
            }
            DomainError::Database(db_err) => {
                tracing::error!(
                    kind = db_error_kind(&db_err),
                    "github-mirror database error"
                );
                CanonicalError::internal(INTERNAL_DETAIL).create()
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panic in these tests is the failure report"
)]
mod tests {
    use super::*;

    fn status_of(e: DomainError) -> u16 {
        CanonicalError::from(e).status_code()
    }

    /// What a caller actually receives, so two errors can be compared as the
    /// caller sees them and not just by status code.
    fn body_of(e: DomainError) -> String {
        let problem = Problem::from_error(&CanonicalError::from(e)).unwrap();
        serde_json::to_string(&problem).unwrap()
    }

    #[test]
    fn every_domain_error_maps_to_its_canonical_status() {
        assert_eq!(status_of(DomainError::NotFound), 404);
        assert_eq!(
            status_of(DomainError::Validation {
                field: "owner".to_owned(),
                message: "empty".to_owned(),
            }),
            400
        );
        // Both "caller lacks rights" and "the mirror's own upstream access
        // is gone" must read as 404: a 403 would confirm the repo exists.
        assert_eq!(status_of(DomainError::forbidden("no scope")), 404);
        assert_eq!(
            status_of(DomainError::AccessLost("token revoked".to_owned())),
            404
        );
        assert_eq!(
            status_of(DomainError::Conflict("sync already running".to_owned())),
            409
        );
        assert_eq!(status_of(DomainError::internal("boom")), 500);
        assert_eq!(
            status_of(DomainError::Database(toolkit_db::DbError::InvalidConfig(
                "bad dsn".to_owned()
            ))),
            500
        );
    }

    #[test]
    fn a_validation_error_carries_the_field_in_its_body() {
        let body = body_of(DomainError::Validation {
            field: "since".to_owned(),
            message: "not an RFC3339 timestamp".to_owned(),
        });
        assert!(
            body.contains("\"field\":\"since\""),
            "the compat router turns this into GitHub's `errors[]`: {body}"
        );
    }

    #[test]
    fn forbidden_and_access_lost_are_indistinguishable_to_the_caller() {
        assert_eq!(
            body_of(DomainError::forbidden("tenant has no scope")),
            body_of(DomainError::AccessLost("token revoked".to_owned())),
            "a caller must not be able to tell a private repository from a \
             missing one, or either from the mirror losing its own access"
        );
    }
}
