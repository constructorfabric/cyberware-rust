use toolkit_db::DbError;
use toolkit_macros::domain_model;

#[domain_model]
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("Settings not found")]
    NotFound,

    #[error("Validation error on field '{field}': {message}")]
    Validation { field: String, message: String },

    #[error("Access forbidden: {0}")]
    Forbidden(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Database error: {0}")]
    Database(#[from] DbError),
}

impl DomainError {
    pub fn validation(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Validation {
            field: field.into(),
            message: message.into(),
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::Forbidden(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

// TODO(DE1302): `DomainError::Forbidden` and `DomainError::Internal` only carry
// Strings, so the `EnforcerError` source is lost. Extend the variants to hold a
// boxed source so `.source()` returns the original error, then remove this allow.
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

/// A resolver that could not answer is an internal failure, not a denial.
///
/// The gear asked a collaborator "whose settings are these?" and got no answer;
/// carrying on with the token subject would file the caller's settings under a
/// key their next request may not produce, so the read fails instead.
#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<toolkit_canonical_errors::CanonicalError> for DomainError {
    fn from(e: toolkit_canonical_errors::CanonicalError) -> Self {
        tracing::error!(error = %e, "settings owner resolution failed");
        Self::Internal(e.to_string())
    }
}
