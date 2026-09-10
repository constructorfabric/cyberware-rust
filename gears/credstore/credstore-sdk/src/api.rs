//! Consumer contract for tenant-scoped credential operations.
//!
//! Defines anti-enumerating reads and explicit optimistic-concurrency semantics
//! for create, update, and delete operations.

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::error::CredStoreError;
use crate::models::{
    GetSecretResponse, SecretRef, SecretValue, SharingMode, WriteOptions, WritePrecondition,
};

/// Consumer-facing API trait for credential storage operations.
#[async_trait]
pub trait CredStoreClientV1: Send + Sync {
    /// Retrieves a secret by reference, applying hierarchical resolution.
    ///
    /// Returns `Ok(Some(_))` with the value and metadata when an accessible
    /// secret is found, `Ok(None)` when none exists or is inaccessible (a
    /// single 404 surface that prevents enumeration), and
    /// `Err(AccessDenied)` only when the caller lacks read permission.
    async fn get(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<GetSecretResponse>, CredStoreError>;

    /// Updates an existing secret with default [`WriteOptions`]: the type is
    /// preserved and so is the expiry
    /// ([`ExpiryWrite::Preserve`](crate::ExpiryWrite::Preserve)), so a value
    /// rotation never strips an existing expiry. Use [`Self::put_opts`] to set
    /// or clear it explicitly.
    ///
    /// # Concurrency
    ///
    /// Every write names its concurrency stance — `precondition` is required,
    /// there is no unconditional overwrite:
    ///
    /// * Read-modify-write callers (must not overwrite a concurrent update)
    ///   pass [`WritePrecondition::Matches`] with the `(id, version)` from the
    ///   [`GetSecretResponse`] they derived the new value from — the
    ///   in-process `If-Match` — and handle [`CredStoreError::Conflict`] by
    ///   re-reading. The generation-bound validator also closes the ABA hole
    ///   (a validator from a deleted-and-recreated secret never matches).
    /// * Blind create-or-replace flows (rotation / provisioning controllers
    ///   that own their references, where the new value is not derived from
    ///   the stored one) pass [`WritePrecondition::Exists`] — an explicit
    ///   last-writer-wins overwrite, `create` + retry when the secret may not
    ///   exist yet. Under immutable value versions (ADR-0006) `Exists` means
    ///   exactly ordinary last-writer-wins with no other role: recovering a
    ///   fence-poisoned reference (ADR-0003) is an ordinary new write too
    ///   (any precondition), since there is no "same key" left to re-`PUT`
    ///   into — a fenced `GET` still fails closed with `Ok(None)`, but no
    ///   special healing path exists to obtain a version validator from.
    ///
    /// A `put` never creates: the target must exist, and a missing target
    /// fails the precondition with [`CredStoreError::Conflict`] regardless of
    /// the variant. Use [`Self::create`] for the create path (the only
    /// preconditionless write).
    async fn put(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        value: SecretValue,
        sharing: SharingMode,
        precondition: WritePrecondition,
    ) -> Result<(), CredStoreError> {
        self.put_opts(
            ctx,
            key,
            value,
            sharing,
            precondition,
            WriteOptions::default(),
        )
        .await
    }

    /// Updates an existing secret with explicit [`WriteOptions`] (secret type,
    /// expiry). The type is immutable: an `opts.secret_type` differing from
    /// the existing secret's type is rejected. A failed `precondition` yields
    /// [`CredStoreError::Conflict`] — see [`Self::put`] for the concurrency
    /// contract.
    ///
    /// The default implementation reports the operation as unsupported so
    /// value-store test doubles that only override [`Self::get`] stay valid;
    /// real gear clients override it.
    async fn put_opts(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        value: SecretValue,
        sharing: SharingMode,
        precondition: WritePrecondition,
        opts: WriteOptions,
    ) -> Result<(), CredStoreError> {
        let _ = (ctx, key, value, sharing, precondition, opts);
        Err(CredStoreError::internal(
            "put_opts is not supported by this CredStoreClientV1 implementation",
        ))
    }

    /// Creates a secret, failing with [`CredStoreError::Conflict`] if one of the
    /// same sharing class already exists (create-only — the 409 path behind the
    /// REST `POST`, and the only write without a precondition). Use
    /// [`Self::put`] to update an existing secret.
    async fn create(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        value: SecretValue,
        sharing: SharingMode,
    ) -> Result<(), CredStoreError> {
        self.create_opts(ctx, key, value, sharing, WriteOptions::default())
            .await
    }

    /// Create-only variant of [`Self::put_opts`]. See it for the default
    /// implementation contract.
    async fn create_opts(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        value: SecretValue,
        sharing: SharingMode,
        opts: WriteOptions,
    ) -> Result<(), CredStoreError> {
        let _ = (ctx, key, value, sharing, opts);
        Err(CredStoreError::internal(
            "create_opts is not supported by this CredStoreClientV1 implementation",
        ))
    }

    /// Deletes a secret, guarded by a required optimistic-concurrency
    /// [`WritePrecondition`] (the in-process equivalent of a REST `If-Match`
    /// delete; [`WritePrecondition::Exists`] is the explicit
    /// delete-whatever-is-there form). A failed precondition yields
    /// [`CredStoreError::Conflict`].
    ///
    /// The default implementation reports the operation as unsupported so
    /// value-store test doubles that only override [`Self::get`] stay valid;
    /// real gear clients override it.
    async fn delete(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        precondition: WritePrecondition,
    ) -> Result<(), CredStoreError> {
        let _ = (ctx, key, precondition);
        Err(CredStoreError::internal(
            "delete is not supported by this CredStoreClientV1 implementation",
        ))
    }
}
