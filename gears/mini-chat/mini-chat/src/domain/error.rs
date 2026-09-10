use thiserror::Error;
use toolkit_db::DbError;
use toolkit_db::secure::InfraError;
use toolkit_db::secure::ScopeError;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// Domain-specific errors for the mini-chat gear.
#[domain_model]
#[derive(Error, Debug)]
pub enum DomainError {
    #[error("Chat not found: {id}")]
    ChatNotFound { id: Uuid },

    #[error("Invalid model: {model}")]
    InvalidModel { model: String },

    #[error("Validation failed: {message}")]
    Validation { message: String },

    #[error("Database error: {message}")]
    Database { message: String },

    #[error("Conflict: {code}: {message}")]
    Conflict { code: String, message: String },

    #[error("{entity} not found: {id}")]
    NotFound { entity: String, id: Uuid },

    #[error("Access denied")]
    Forbidden,

    #[error("Message not found: {id}")]
    MessageNotFound { id: Uuid },

    #[error("Invalid reaction target: message {id} is not an assistant message")]
    InvalidReactionTarget { id: Uuid },

    #[error("Model not found: {model_id}")]
    ModelNotFound { model_id: String },

    #[error("Internal error: {message}")]
    InternalError { message: String },

    #[error("Web search is currently disabled")]
    WebSearchDisabled,

    #[error("Web search calls exceeded for this message")]
    WebSearchCallsExceeded,

    #[error("Unsupported file type: {mime}")]
    UnsupportedFileType { mime: String },

    #[error("File too large: {message}")]
    FileTooLarge { message: String },

    #[error("Document limit exceeded: {message}")]
    DocumentLimitExceeded { message: String },

    #[error("Storage limit exceeded: {message}")]
    StorageLimitExceeded { message: String },

    #[error("Service temporarily unavailable: {message}")]
    ServiceUnavailable { message: String },

    /// Provider returned an error. `sanitized_message` is pre-sanitized by
    /// `sanitize_provider_message()` at construction — safe for client exposure.
    #[error("Provider error: {sanitized_message}")]
    ProviderError {
        code: String,
        sanitized_message: String,
    },
}

impl DomainError {
    #[must_use]
    pub fn chat_not_found(id: Uuid) -> Self {
        Self::ChatNotFound { id }
    }

    #[must_use]
    pub fn invalid_model(model: impl Into<String>) -> Self {
        Self::InvalidModel {
            model: model.into(),
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation {
            message: message.into(),
        }
    }

    pub fn database(message: impl Into<String>) -> Self {
        Self::Database {
            message: message.into(),
        }
    }

    pub fn conflict(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Conflict {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn not_found(entity: impl Into<String>, id: Uuid) -> Self {
        Self::NotFound {
            entity: entity.into(),
            id,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::InternalError {
            message: message.into(),
        }
    }

    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self::ServiceUnavailable {
            message: message.into(),
        }
    }

    #[must_use]
    pub fn message_not_found(id: Uuid) -> Self {
        Self::MessageNotFound { id }
    }

    #[must_use]
    pub fn invalid_reaction_target(id: Uuid) -> Self {
        Self::InvalidReactionTarget { id }
    }

    #[must_use]
    pub fn model_not_found(model_id: impl Into<String>) -> Self {
        Self::ModelNotFound {
            model_id: model_id.into(),
        }
    }

    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn database_infra(e: InfraError) -> Self {
        Self::database(e.to_string())
    }
}

impl From<Box<dyn std::error::Error>> for DomainError {
    fn from(value: Box<dyn std::error::Error>) -> Self {
        tracing::debug!(error = %value, "Converting boxed error to DomainError");
        DomainError::internal(value.to_string())
    }
}

/// Helper to convert any displayable error into `DomainError::Database`.
pub fn db_err(e: impl std::fmt::Display) -> DomainError {
    DomainError::database(e.to_string())
}

// TODO(DE1302): `DomainError::database(...)` only accepts a String, so the
// source `DbError` is dropped. Extend `Database` to hold the source error and
// remove this allow.
#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<DbError> for DomainError {
    fn from(e: DbError) -> Self {
        DomainError::database(e.to_string())
    }
}

impl From<ScopeError> for DomainError {
    #[allow(clippy::cognitive_complexity)]
    fn from(e: ScopeError) -> Self {
        match e {
            ScopeError::Db(ref db_err) => map_db_err(db_err),
            ScopeError::Denied(msg) => {
                tracing::warn!("scope denied: {msg}");
                DomainError::Forbidden
            }
            ScopeError::TenantNotInScope { tenant_id } => {
                tracing::warn!("tenant {tenant_id} not in scope");
                DomainError::Forbidden
            }
            ScopeError::Invalid(msg) => {
                tracing::error!("invalid scope: {msg}");
                DomainError::internal(msg)
            }
            // `ScopeError` is `#[non_exhaustive]`: variants this gear has no
            // specific answer for (today the graph-query refusals, which it can
            // never trigger) map to an internal error, like `Invalid`.
            other => {
                tracing::error!("invalid scope: {other}");
                DomainError::internal(format!("invalid scope: {other}"))
            }
        }
    }
}

// TODO(DE1302): `DomainError::internal(...)` only accepts a String, so the
// source `EnforcerError` is dropped. Extend the variant to hold the source and
// remove this allow.
#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<authz_resolver_sdk::EnforcerError> for DomainError {
    #[allow(clippy::cognitive_complexity)]
    fn from(e: authz_resolver_sdk::EnforcerError) -> Self {
        match e {
            authz_resolver_sdk::EnforcerError::Denied { ref deny_reason } => {
                tracing::warn!(deny_reason = ?deny_reason, "AuthZ denied access");
                Self::Forbidden
            }
            authz_resolver_sdk::EnforcerError::CompileFailed(ref err) => {
                tracing::warn!(error = %err, "AuthZ constraint compile failed - access denied");
                Self::Forbidden
            }
            authz_resolver_sdk::EnforcerError::EvaluationFailed(ref err) => {
                tracing::error!(error = %err, "AuthZ evaluation failed (internal error)");
                Self::internal(err.to_string())
            }
        }
    }
}

fn map_db_err(db_err: &sea_orm::DbErr) -> DomainError {
    if let Some(sea_orm::SqlErr::UniqueConstraintViolation(msg)) = db_err.sql_err() {
        return DomainError::Conflict {
            code: "unique_violation".into(),
            message: msg,
        };
    }
    // Fallback: SeaORM's sql_err() may fail to classify the violation when
    // the error is wrapped by a connection proxy or driver layer. Use the
    // robust string-based detector from toolkit-db.
    if toolkit_db::secure::is_unique_violation(db_err) {
        return DomainError::Conflict {
            code: "unique_violation".into(),
            message: db_err.to_string(),
        };
    }
    DomainError::database(db_err.to_string())
}
