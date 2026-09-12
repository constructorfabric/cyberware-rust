use toolkit_macros::domain_model;

#[domain_model]
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("Repo not found")]
    NotFound,

    #[error("Validation error on field '{field}': {message}")]
    Validation { field: String, message: String },

    #[error("Access forbidden: {0}")]
    Forbidden(String),

    /// GitHub refused the mirror's own credentials for an upstream resource
    /// (401, or 403 that is not a rate limit) - the repo went private, the
    /// token was revoked, or its scopes shrank. Distinct from [`Self::Forbidden`]
    /// (the *caller* lacks rights) so operators can tell which side lost access.
    #[error("GitHub access lost: {0}")]
    AccessLost(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Database error: {0}")]
    Database(#[from] toolkit_db::DbError),
}

impl DomainError {
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::Forbidden(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Database(toolkit_db::DbError::Sea(e)) => [
                sea_orm::DbBackend::Sqlite,
                sea_orm::DbBackend::Postgres,
                sea_orm::DbBackend::MySql,
            ]
            .into_iter()
            .any(|backend| toolkit_db::contention::is_retryable_contention(backend, e)),
            _ => false,
        }
    }
}

#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<authz_resolver_sdk::EnforcerError> for DomainError {
    fn from(e: authz_resolver_sdk::EnforcerError) -> Self {
        tracing::error!(error = %e, "AuthZ scope resolution failed");
        match e {
            authz_resolver_sdk::EnforcerError::Denied { .. }
            | authz_resolver_sdk::EnforcerError::CompileFailed(_) => Self::Forbidden(e.to_string()),
            authz_resolver_sdk::EnforcerError::EvaluationFailed(_) => Self::Internal(e.to_string()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn constructors_carry_their_messages() {
        assert_eq!(
            (DomainError::Validation {
                field: "title".to_owned(),
                message: "too long".to_owned()
            })
            .to_string(),
            "Validation error on field 'title': too long"
        );
        assert_eq!(
            DomainError::forbidden("nope").to_string(),
            "Access forbidden: nope"
        );
        assert_eq!(
            DomainError::internal("boom").to_string(),
            "Internal error: boom"
        );
        assert_eq!(DomainError::NotFound.to_string(), "Repo not found");
    }

    #[test]
    fn enforcer_denial_maps_to_forbidden() {
        let denied = authz_resolver_sdk::EnforcerError::Denied { deny_reason: None };
        assert!(matches!(
            DomainError::from(denied),
            DomainError::Forbidden(_)
        ));
    }

    #[test]
    fn enforcer_evaluation_failure_maps_to_internal() {
        let failed = authz_resolver_sdk::EnforcerError::EvaluationFailed(
            toolkit::api::canonical_prelude::CanonicalError::internal("pdp unreachable").create(),
        );
        assert!(matches!(
            DomainError::from(failed),
            DomainError::Internal(_)
        ));
    }
}
