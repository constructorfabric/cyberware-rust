//! Pinning tests for [`DomainError`].
//!
//! Boundary-mapping tests (`DomainError` → SDK / `CanonicalError`) live
//! next to the mappings themselves under `api/service/` and
//! `api/rest/`.

use std::error::Error as _;
use std::time::Duration;

use uuid::Uuid;

use super::*;

#[test]
fn convenience_constructors_match_variants() {
    let svc = DomainError::service_unavailable("pool timeout");
    assert!(matches!(
        svc,
        DomainError::ServiceUnavailable {
            ref detail,
            retry_after: None,
            cause: None,
        } if detail == "pool timeout"
    ));

    let internal = DomainError::internal("boom");
    assert!(matches!(
        internal,
        DomainError::Internal {
            ref diagnostic,
            cause: None,
        } if diagnostic == "boom"
    ));

    let denied = DomainError::authorization_denied("no");
    assert!(matches!(
        denied,
        DomainError::AuthorizationDenied {
            ref detail,
            cause: None,
        } if detail == "no"
    ));
}

#[test]
fn internal_display_does_not_leak_diagnostic_or_cause() {
    // The `Display` impl for `Internal` is deliberately opaque so
    // tracing layers that log `%err` cannot exfiltrate diagnostic text
    // or upstream `cause` strings without going through the audit
    // formatter.
    let cause: super::BoxError = std::sync::Arc::new(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "secret-host:5432: connection refused",
    ));
    let err = DomainError::Internal {
        diagnostic: "internal classifier ran out of arms".to_owned(),
        cause: Some(cause),
    };
    let rendered = err.to_string();
    assert!(!rendered.contains("secret-host"), "rendered = {rendered}");
    assert!(
        !rendered.contains("internal classifier"),
        "rendered = {rendered}"
    );
    // `source()` chain still reachable for the audit log.
    assert!(err.source().is_some());
}

/// `Clone` on a `DomainError::Internal` carrying a cause MUST preserve the
/// source chain. With `Box<dyn Error>` — which is not `Clone` — the cause would
/// be dropped every time the error round-tripped through a test stub or a retry
/// classifier.
#[test]
fn clone_preserves_cause_chain_on_internal() {
    let cause: super::BoxError = std::sync::Arc::new(std::io::Error::other("upstream failed"));
    let err = DomainError::Internal {
        diagnostic: "internal classifier".to_owned(),
        cause: Some(cause),
    };
    let cloned = err.clone();
    assert!(
        cloned.source().is_some(),
        "cloned DomainError::Internal MUST still carry the cause chain"
    );
    // The ORIGINAL is untouched: `Clone` must not move the cause out of it.
    assert!(err.source().is_some());
}

/// Same invariant for `ServiceUnavailable`.
#[test]
fn clone_preserves_cause_chain_on_service_unavailable() {
    let cause: super::BoxError = std::sync::Arc::new(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "upstream down",
    ));
    let err = DomainError::ServiceUnavailable {
        detail: "pdp transport".to_owned(),
        retry_after: Some(std::time::Duration::from_secs(2)),
        cause: Some(cause),
    };
    let cloned = err.clone();
    assert!(
        cloned.source().is_some(),
        "cloned DomainError::ServiceUnavailable MUST still carry the cause chain"
    );
    assert!(err.source().is_some());
}

/// Same invariant for `AuthorizationDenied`.
#[test]
fn clone_preserves_cause_chain_on_authorization_denied() {
    let cause: super::BoxError = std::sync::Arc::new(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "policy denied",
    ));
    let err = DomainError::AuthorizationDenied {
        detail: "denied".to_owned(),
        cause: Some(cause),
    };
    let cloned = err.clone();
    assert!(
        cloned.source().is_some(),
        "cloned DomainError::AuthorizationDenied MUST still carry the cause chain"
    );
    assert!(err.source().is_some());
}

#[test]
fn service_unavailable_carries_retry_after_when_supplied() {
    let err = DomainError::ServiceUnavailable {
        detail: "pdp transport".to_owned(),
        retry_after: Some(Duration::from_secs(7)),
        cause: None,
    };
    assert!(matches!(
        err,
        DomainError::ServiceUnavailable {
            retry_after: Some(d),
            ..
        } if d == Duration::from_secs(7)
    ));
}

#[test]
fn role_definition_not_found_carries_id() {
    let id = Uuid::nil();
    let err = DomainError::RoleDefinitionNotFound { id };
    assert!(err.to_string().contains(&id.to_string()));
}
