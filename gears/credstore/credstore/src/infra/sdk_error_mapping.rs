//! `DomainError` → [`CanonicalError`] boundary mapping for the credstore REST layer.

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::DomainError;
use crate::domain::secret::typing::reasons;

// ---------------------------------------------------------------------------
// Resource marker
// ---------------------------------------------------------------------------

#[resource_error(gts_id!("cf.core.credstore.credential.v1~"))]
pub(crate) struct CredentialResource;

// ---------------------------------------------------------------------------
// DomainError → CanonicalError
// ---------------------------------------------------------------------------

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::InvalidSecretRef { detail } => CredentialResource::invalid_argument()
                .with_field_violation("reference", detail, "INVALID_SECRET_REF")
                .create(),
            DomainError::NotFound => CredentialResource::not_found("credential not found")
                .with_resource("credential")
                .create(),
            DomainError::Conflict => {
                CredentialResource::already_exists("credential already exists")
                    .with_resource("credential")
                    .create()
            }
            // No 412 in the canonical model; optimistic-lock conflicts are Aborted (409).
            DomainError::VersionConflict => {
                CredentialResource::aborted("credential version precondition failed")
                    .with_reason("OPTIMISTIC_LOCK_FAILURE")
                    .create()
            }
            DomainError::InvalidPrecondition { detail } => CredentialResource::invalid_argument()
                .with_field_violation("If-Match", detail, "INVALID_IF_MATCH")
                .create(),
            // No 428 in the canonical model; a missing mandatory `If-Match` is
            // a request the client must fix → invalid_argument (400) with its
            // own reason so clients can tell "absent" from "malformed".
            DomainError::PreconditionRequired { detail } => CredentialResource::invalid_argument()
                .with_field_violation("If-Match", detail, "IF_MATCH_REQUIRED")
                .create(),
            // ADR-0004: the two type-conflict reasons are canonical `Aborted`
            // (409, like a version conflict — the request contended with
            // state it didn't expect); every other trait-violation reason
            // stays `InvalidArgument` (400).
            DomainError::TypeViolation { reason, detail, .. }
                if reason == reasons::TYPE_IMMUTABLE
                    || reason == reasons::TYPE_MISMATCH_WITH_INHERITED =>
            {
                CredentialResource::aborted(detail)
                    .with_reason(reason)
                    .create()
            }
            // Both are plain "the request body/field is wrong" shapes;
            // `TypeViolation` just names its own reason constants.
            DomainError::TypeViolation {
                field,
                reason,
                detail,
            }
            | DomainError::InvalidRequest {
                field,
                reason,
                detail,
            } => CredentialResource::invalid_argument()
                .with_field_violation(field, detail, reason)
                .create(),
            DomainError::UnsupportedTransition { detail } => {
                CredentialResource::failed_precondition()
                    .with_precondition_violation("sharing", detail, "UNSUPPORTED_TRANSITION")
                    .create()
            }
            DomainError::AccessDenied { .. } => CredentialResource::permission_denied()
                .with_reason("ACCESS_DENIED")
                .create(),
            DomainError::ServiceUnavailable {
                detail,
                retry_after,
                ..
            } => {
                let mut builder = CanonicalError::service_unavailable().with_detail(detail);
                if let Some(duration) = retry_after {
                    builder = builder.with_retry_after_seconds(duration.as_secs());
                }
                builder.create()
            }
            DomainError::Internal { diagnostic, .. } => {
                CanonicalError::internal(diagnostic).create()
            }
            #[allow(unreachable_patterns)]
            other => {
                CanonicalError::internal(format!("unmapped DomainError variant: {other}")).create()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use toolkit_canonical_errors::CanonicalError;

    use crate::domain::error::DomainError;

    fn status_of(err: DomainError) -> u16 {
        CanonicalError::from(err).status_code()
    }

    #[test]
    fn every_variant_maps_to_a_client_or_server_error() {
        assert_eq!(status_of(DomainError::NotFound), 404);
        assert_eq!(status_of(DomainError::Conflict), 409);
        assert_eq!(
            status_of(DomainError::InvalidSecretRef {
                detail: "bad".to_owned()
            }),
            400
        );
        // Exact pins (not `>= 400`): the handler docs promise these specific
        // codes, so a regression that mapped them to 500 must fail the suite
        // (review finding #14).
        assert_eq!(
            status_of(DomainError::UnsupportedTransition {
                detail: "no".to_owned()
            }),
            400
        );
        assert_eq!(status_of(DomainError::AccessDenied { cause: None }), 403);
        assert_eq!(status_of(DomainError::internal("boom")), 500);
        // No 412 in the canonical model: optimistic-lock conflicts are Aborted (409).
        assert_eq!(status_of(DomainError::VersionConflict), 409);
        assert_eq!(
            status_of(DomainError::InvalidPrecondition {
                detail: "bad".to_owned()
            }),
            400
        );
        assert_eq!(
            status_of(DomainError::PreconditionRequired {
                detail: "missing".to_owned()
            }),
            400
        );
    }

    #[test]
    fn type_immutable_and_type_mismatch_with_inherited_are_aborted_409() {
        use crate::domain::secret::typing::reasons;
        for reason in [
            reasons::TYPE_IMMUTABLE,
            reasons::TYPE_MISMATCH_WITH_INHERITED,
        ] {
            assert_eq!(
                status_of(DomainError::TypeViolation {
                    field: "type",
                    reason,
                    detail: "x".to_owned(),
                }),
                409,
                "{reason} must map to 409"
            );
        }
    }

    #[test]
    fn other_type_violation_reasons_stay_invalid_argument_400() {
        use crate::domain::secret::typing::reasons;
        assert_eq!(
            status_of(DomainError::TypeViolation {
                field: "sharing",
                reason: reasons::SHARING_NOT_ALLOWED_FOR_TYPE,
                detail: "x".to_owned(),
            }),
            400
        );
    }

    #[test]
    fn invalid_request_reasons_are_400() {
        use crate::domain::secret::typing::reasons;
        for reason in [
            reasons::VALUE_REQUIRED,
            reasons::EMPTY_PATCH,
            reasons::NULL_NOT_ALLOWED,
            reasons::PRECONDITION_REQUIRED,
            reasons::TYPE_REQUIRED,
        ] {
            assert_eq!(
                status_of(DomainError::InvalidRequest {
                    field: "value",
                    reason,
                    detail: "x".to_owned(),
                }),
                400,
                "{reason} must map to 400"
            );
        }
    }

    #[test]
    fn service_unavailable_carries_retry_after() {
        assert_eq!(
            status_of(DomainError::ServiceUnavailable {
                detail: "later".to_owned(),
                retry_after: Some(Duration::from_secs(30)),
                cause: None,
            }),
            503
        );
        // Without retry_after the other branch is taken.
        assert_eq!(
            status_of(DomainError::ServiceUnavailable {
                detail: "later".to_owned(),
                retry_after: None,
                cause: None,
            }),
            503
        );
    }

    #[test]
    fn resource_error_string_matches_sdk_constant() {
        // The `#[resource_error(...)]` literal on `CredentialResource` must equal
        // the SDK's single source of truth
        // (`credstore_sdk::CREDENTIAL_RESOURCE_TYPE`); a divergence trips here at
        // test time, not in production. NotFound goes through the
        // `CredentialResource` marker, so the built error carries the type.
        let err = CanonicalError::from(DomainError::NotFound);
        assert_eq!(
            err.resource_type(),
            Some(credstore_sdk::CREDENTIAL_RESOURCE_TYPE)
        );
    }
}
