//! Write wrapper over [`credstore_sdk::CredStoreClientV1`].
//!
//! The plugin's system [`SecurityContext`] is baked in at construction so call
//! sites cannot accidentally pass an AM-forwarded ctx to mutation paths
//! (DESIGN "Component Model", "Security and Data Protection"). Vendor 404 on `delete` is mapped to `Ok(())` —
//! "already absent" is success-equivalent (DESIGN "Component Model").

use std::sync::Arc;

use credstore_sdk::{
    CredStoreClientV1, CredStoreError, CredentialWrite, Fallback, PutPrecondition, SecretRef,
    SecretType, SecretValue, SharingMode, WritePrecondition,
};
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;

/// Ctx-baked-in write wrapper over [`CredStoreClientV1`].
#[domain_model]
#[derive(Clone)]
pub struct CredStoreWriter {
    inner: Arc<dyn CredStoreClientV1>,
    system_ctx: Arc<SecurityContext>,
}

impl CredStoreWriter {
    /// Wrap a [`CredStoreClientV1`] with the plugin-owned system ctx.
    #[must_use]
    pub fn new(inner: Arc<dyn CredStoreClientV1>, system_ctx: Arc<SecurityContext>) -> Self {
        Self { inner, system_ctx }
    }

    /// Create or replace one secret.
    ///
    /// ADR-0004's `put` never creates on a missing target when guarded by
    /// [`PutPrecondition::Exists`] — it fails the precondition with
    /// [`CredStoreError::Conflict`] instead. So create-or-replace is a
    /// [`PutPrecondition::CreateOnly`] attempt first (with `secret_type` set
    /// to `generic`, since a create must name a type), falling back to a
    /// [`PutPrecondition::Exists`] overwrite (which must *not* repeat the
    /// type, since `put` treats a replace's `secret_type` as an immutability
    /// check against the stored type) when the reference already exists.
    /// `Exists` is the documented opt-out for exactly this shape: a
    /// provisioning writer that owns its references and whose new value is
    /// not derived from the stored one, so there is no validator to carry.
    /// Concurrent writers to one reference therefore race last-writer-wins,
    /// which is the pre-existing behaviour of this path.
    ///
    /// # Errors
    ///
    /// Any [`CredStoreError`] surfaced by the underlying client (transport,
    /// auth, validation).
    pub async fn put(
        &self,
        key: &SecretRef,
        value: SecretValue,
        sharing: SharingMode,
    ) -> Result<(), CredStoreError> {
        // `SecretValue` is a non-Clone newtype over the raw bytes, so keep a
        // copy to rebuild it for the overwrite leg instead of cloning.
        let bytes = value.as_bytes().to_vec();
        let create = CredentialWrite {
            secret_type: Some(SecretType::generic().into()),
            sharing,
            fallback: Fallback::Inherit,
            expires_at: None,
            value,
        };
        match self
            .inner
            .put(&self.system_ctx, key, create, PutPrecondition::CreateOnly)
            .await
        {
            Ok(_) => Ok(()),
            // Already present: fall through to the explicit overwrite.
            Err(CredStoreError::Conflict) => {
                let replace = CredentialWrite {
                    secret_type: None,
                    sharing,
                    fallback: Fallback::Inherit,
                    expires_at: None,
                    value: SecretValue::new(bytes),
                };
                self.inner
                    .put(&self.system_ctx, key, replace, PutPrecondition::Exists)
                    .await
                    .map(|_| ())
            }
            Err(e) => Err(e),
        }
    }

    /// Delete one plugin-owned secret. Vendor 404
    /// ([`CredStoreError::NotFound`]) is mapped to `Ok(())` — "already
    /// absent" is success-equivalent on the deprovision path (DESIGN "Component Model").
    ///
    /// # Errors
    ///
    /// Any non-`NotFound` [`CredStoreError`] surfaced by the underlying
    /// client.
    pub async fn delete(&self, key: &SecretRef) -> Result<(), CredStoreError> {
        // `Exists` is the explicit delete-whatever-is-there form; the
        // plugin owns these references and holds no version validator.
        match self
            .inner
            .delete(&self.system_ctx, key, WritePrecondition::Exists)
            .await
        {
            Ok(()) | Err(CredStoreError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod writer_tests;
