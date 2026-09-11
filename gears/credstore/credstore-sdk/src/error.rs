//! Stable SDK error taxonomy shared by consumers and storage plugins.

use std::time::Duration;

use thiserror::Error;

/// Errors that can occur during credential store operations.
#[derive(Debug, Error)]
pub enum CredStoreError {
    #[error("invalid secret reference: {reason}")]
    InvalidSecretRef { reason: String },
    #[error("access denied")]
    AccessDenied,
    #[error("secret not found")]
    NotFound,
    #[error("secret already exists")]
    Conflict,
    #[error("no plugin available")]
    NoPluginAvailable,
    #[error("service unavailable: {detail}")]
    ServiceUnavailable {
        detail: String,
        retry_after: Option<Duration>,
    },
    #[error("unsupported sharing transition: {detail}")]
    UnsupportedTransition { detail: String },
    /// A write violated the secret type's traits (unknown type, disallowed
    /// sharing mode, schema/size/format violation, expiry on a
    /// non-expirable type), or named a type that conflicts with the one
    /// already in play: a differing type on replace (`TYPE_IMMUTABLE`) or a
    /// create over a reference that currently resolves to an ancestor's
    /// `shared` record of a different type (`TYPE_MISMATCH_WITH_INHERITED`).
    /// `reason` is a stable machine-readable code.
    #[error("secret type violation ({reason}): {detail}")]
    TypeViolation { reason: String, detail: String },
    /// A request is malformed independently of any secret type — an empty
    /// merge patch (`EMPTY_PATCH`), a required `value` missing from a `PUT`
    /// (`VALUE_REQUIRED`), a merge-patch `null` on a non-nullable field
    /// (`NULL_NOT_ALLOWED`), or a missing/conflicting write precondition
    /// (`PRECONDITION_REQUIRED`). `reason` is a stable machine-readable code.
    #[error("invalid request ({reason}): {detail}")]
    InvalidRequest { reason: String, detail: String },
    #[error("internal error: {0}")]
    Internal(String),
}

impl CredStoreError {
    #[must_use]
    pub fn invalid_ref(reason: impl Into<String>) -> Self {
        Self::InvalidSecretRef {
            reason: reason.into(),
        }
    }
    #[must_use]
    pub fn unsupported_transition(detail: impl Into<String>) -> Self {
        Self::UnsupportedTransition {
            detail: detail.into(),
        }
    }
    #[must_use]
    pub fn service_unavailable(detail: impl Into<String>) -> Self {
        Self::ServiceUnavailable {
            detail: detail.into(),
            retry_after: None,
        }
    }
    #[must_use]
    pub fn service_unavailable_with_retry(
        detail: impl Into<String>,
        retry_after: Duration,
    ) -> Self {
        Self::ServiceUnavailable {
            detail: detail.into(),
            retry_after: Some(retry_after),
        }
    }
    #[must_use]
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    // ── category predicates (collapse variants for call-site handling) ──────────

    /// `true` for the not-found condition.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound)
    }

    /// `true` for any transient infrastructure outage where retry is appropriate.
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        matches!(
            self,
            Self::ServiceUnavailable { .. } | Self::NoPluginAvailable
        )
    }

    /// `true` if the operation may succeed on a future retry.
    ///
    /// Narrower than [`Self::is_unavailable`]: `NoPluginAvailable` is an
    /// operator misconfiguration (no backend plugin registered), not a
    /// transient outage, so it is reported as unavailable but **not**
    /// retryable — retrying without a config change cannot succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::ServiceUnavailable { .. })
    }

    /// `true` for request-shape rejections (invalid secret reference,
    /// secret-type trait violations, malformed request shape).
    #[must_use]
    pub fn is_validation_error(&self) -> bool {
        matches!(
            self,
            Self::InvalidSecretRef { .. }
                | Self::TypeViolation { .. }
                | Self::InvalidRequest { .. }
        )
    }

    /// `true` for state-precondition failures (unsupported sharing transition).
    #[must_use]
    pub fn is_precondition_failed(&self) -> bool {
        matches!(self, Self::UnsupportedTransition { .. })
    }

    /// `true` for duplicate-on-create failures.
    #[must_use]
    pub fn is_already_exists(&self) -> bool {
        matches!(self, Self::Conflict)
    }

    /// `true` for authorization denials.
    #[must_use]
    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::AccessDenied)
    }

    /// Retry-after hint (seconds) for transient outages that carry one. Only
    /// [`Self::ServiceUnavailable`] populates it; other variants return `None`.
    #[must_use]
    pub fn retry_after_seconds(&self) -> Option<u32> {
        match self {
            Self::ServiceUnavailable { retry_after, .. } => {
                retry_after.and_then(|d| u32::try_from(d.as_secs()).ok())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn constructors_build_expected_variants() {
        assert!(matches!(
            CredStoreError::invalid_ref("x"),
            CredStoreError::InvalidSecretRef { .. }
        ));
        assert!(matches!(
            CredStoreError::service_unavailable("down"),
            CredStoreError::ServiceUnavailable {
                retry_after: None,
                ..
            }
        ));
        assert!(matches!(
            CredStoreError::service_unavailable_with_retry("down", Duration::from_secs(5)),
            CredStoreError::ServiceUnavailable {
                retry_after: Some(_),
                ..
            }
        ));
        assert!(matches!(
            CredStoreError::internal("boom"),
            CredStoreError::Internal(_)
        ));
    }

    #[test]
    fn display_redacts_nothing_but_is_stable() {
        assert_eq!(CredStoreError::NotFound.to_string(), "secret not found");
        assert_eq!(CredStoreError::AccessDenied.to_string(), "access denied");
        assert_eq!(
            CredStoreError::Conflict.to_string(),
            "secret already exists"
        );
    }

    #[test]
    fn category_predicates_classify_variants() {
        assert!(CredStoreError::NotFound.is_not_found());
        assert!(CredStoreError::Conflict.is_already_exists());
        assert!(CredStoreError::AccessDenied.is_permission_denied());
        assert!(CredStoreError::invalid_ref("x").is_validation_error());
        assert!(
            CredStoreError::TypeViolation {
                reason: "SHARING_NOT_ALLOWED_FOR_TYPE".to_owned(),
                detail: "x".to_owned(),
            }
            .is_validation_error()
        );
        assert!(CredStoreError::unsupported_transition("x").is_precondition_failed());
        assert!(
            CredStoreError::InvalidRequest {
                reason: "EMPTY_PATCH".to_owned(),
                detail: "x".to_owned(),
            }
            .is_validation_error()
        );
        assert!(CredStoreError::NoPluginAvailable.is_unavailable());
        assert!(CredStoreError::service_unavailable("down").is_unavailable());
        assert!(CredStoreError::service_unavailable("down").is_retryable());
        assert!(!CredStoreError::NotFound.is_retryable());
        // Misconfiguration: unavailable, but not retryable without a config change.
        assert!(!CredStoreError::NoPluginAvailable.is_retryable());
    }

    #[test]
    fn retry_after_seconds_only_for_service_unavailable_with_hint() {
        assert_eq!(
            CredStoreError::service_unavailable_with_retry("x", Duration::from_secs(7))
                .retry_after_seconds(),
            Some(7)
        );
        assert_eq!(
            CredStoreError::service_unavailable("x").retry_after_seconds(),
            None
        );
        assert_eq!(CredStoreError::NotFound.retry_after_seconds(), None);
    }
}
