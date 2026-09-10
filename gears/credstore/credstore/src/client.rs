//! In-process SDK adapter for the credential store.
//!
//! [`CredStoreLocalClient`] maps the public SDK contract onto the domain
//! [`Service`] and translates domain
//! failures into stable SDK errors.

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::{
    CredStoreClientV1, CredStoreError, Credential, CredentialPatch, CredentialWrite, PutOutcome,
    PutPrecondition, Secret, SecretRef, Validator,
};
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::secret::model::{PutPrecondition as DomainPutPrecondition, WritePrecondition};
use crate::domain::secret::service::Service;

/// Map the SDK's `patch`/`delete` optimistic-concurrency precondition onto
/// the domain one. The typed `ClientHub` precondition only expresses the
/// single-generation cases (`Exists` / `Matches`); the multi-validator
/// `AnyVersion` is REST-only.
fn to_domain_precondition(p: credstore_sdk::WritePrecondition) -> WritePrecondition {
    match p {
        credstore_sdk::WritePrecondition::Exists => WritePrecondition::Exists,
        credstore_sdk::WritePrecondition::Matches { id, version } => {
            WritePrecondition::Version { id, version }
        }
    }
}

/// Map the SDK's `put` precondition onto the domain one.
fn to_domain_put_precondition(p: PutPrecondition) -> DomainPutPrecondition {
    match p {
        PutPrecondition::CreateOnly => DomainPutPrecondition::CreateOnly,
        PutPrecondition::Exists => DomainPutPrecondition::Exists,
        PutPrecondition::Matches(Validator { id, version }) => {
            DomainPutPrecondition::Version { id, version }
        }
    }
}

impl From<DomainError> for CredStoreError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound => CredStoreError::NotFound,
            // Both are 409-class; the SDK has no distinct optimistic-lock variant.
            DomainError::Conflict | DomainError::VersionConflict => CredStoreError::Conflict,
            DomainError::InvalidSecretRef { detail } => CredStoreError::invalid_ref(detail),
            DomainError::UnsupportedTransition { detail } => {
                CredStoreError::unsupported_transition(detail)
            }
            DomainError::TypeViolation { reason, detail, .. } => CredStoreError::TypeViolation {
                reason: reason.to_owned(),
                detail,
            },
            DomainError::InvalidRequest { reason, detail, .. } => CredStoreError::InvalidRequest {
                reason: reason.to_owned(),
                detail,
            },
            DomainError::AccessDenied { .. } => CredStoreError::AccessDenied,
            DomainError::ServiceUnavailable {
                detail,
                retry_after,
                ..
            } => CredStoreError::ServiceUnavailable {
                detail,
                retry_after,
            },
            DomainError::Internal { diagnostic, .. } => CredStoreError::internal(diagnostic),
            // The ClientHub API sends only typed, always-valid preconditions
            // (`Exists`/`Matches`); a malformed one can originate only from
            // REST `If-Match` parsing, so `InvalidPrecondition` crossing the
            // in-process boundary is a gear-internal invariant breach.
            DomainError::InvalidPrecondition { detail } => {
                CredStoreError::internal(format!("invalid precondition: {detail}"))
            }
            // The typed SDK makes the precondition a required argument, so the
            // domain's missing-precondition guard can never trip on the
            // in-process path — crossing here is an invariant breach too.
            DomainError::PreconditionRequired { detail } => {
                CredStoreError::internal(format!("precondition required: {detail}"))
            }
            #[allow(unreachable_patterns)]
            other => CredStoreError::internal(format!("unmapped DomainError variant: {other}")),
        }
    }
}

/// In-process [`CredStoreClientV1`] that delegates to the domain [`Service`].
pub struct CredStoreLocalClient {
    svc: Arc<Service>,
}

impl CredStoreLocalClient {
    #[must_use]
    pub fn new(svc: Arc<Service>) -> Self {
        Self { svc }
    }
}

#[async_trait]
impl CredStoreClientV1 for CredStoreLocalClient {
    async fn get(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<Credential>, CredStoreError> {
        self.svc.get(ctx, key).await.map_err(Into::into)
    }

    async fn get_secret(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<Secret>, CredStoreError> {
        match self.svc.get_secret(ctx, key).await {
            // The SDK `get_secret` contract is a single 404 surface:
            // `Ok(None)` covers "does not exist", "inaccessible", and
            // "suppressed" alike. The service's `NotFound` (a resolved row
            // whose backend value is missing even after the read protocol's
            // one retry against its current `value_id` — ADR-0006) is the
            // same surface, so fold it rather than leak an error the
            // contract does not admit.
            Err(DomainError::NotFound) => Ok(None),
            other => other.map_err(Into::into),
        }
    }

    async fn put(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        write: CredentialWrite,
        precondition: PutPrecondition,
    ) -> Result<PutOutcome, CredStoreError> {
        self.svc
            .put(ctx, key, write, to_domain_put_precondition(precondition))
            .await
            .map_err(Into::into)
    }

    async fn patch(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        patch: CredentialPatch,
        precondition: credstore_sdk::WritePrecondition,
    ) -> Result<Validator, CredStoreError> {
        self.svc
            .patch(ctx, key, patch, to_domain_precondition(precondition))
            .await
            .map_err(Into::into)
    }

    async fn delete(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        precondition: credstore_sdk::WritePrecondition,
    ) -> Result<(), CredStoreError> {
        self.svc
            .delete(ctx, key, to_domain_precondition(precondition))
            .await
            .map_err(Into::into)
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod client_tests;
