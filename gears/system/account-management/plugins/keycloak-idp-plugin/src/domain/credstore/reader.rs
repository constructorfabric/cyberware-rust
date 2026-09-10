//! Read wrapper over `dyn CredStoreClientV1`.
//!
//! The plugin's system [`SecurityContext`] is baked in at construction so call
//! sites cannot accidentally pass an AM-forwarded ctx to `inner.get`
//! (DESIGN "Component Model", "Security and Data Protection"). The wrapper exposes a minimal `get` surface that
//! drops the response metadata (`owner_tenant_id`, `sharing`, `is_inherited`)
//! per DESIGN "Component Model" — the plugin only consumes `r.value`.

use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, CredStoreError, SecretRef};
use secrecy::SecretString;
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;

/// Ctx-baked-in read wrapper over [`CredStoreClientV1`].
#[domain_model]
#[derive(Clone)]
pub struct CredStoreReader {
    inner: Arc<dyn CredStoreClientV1>,
    system_ctx: Arc<SecurityContext>,
}

impl CredStoreReader {
    /// Wrap a [`CredStoreClientV1`] with the plugin-owned system ctx.
    #[must_use]
    pub fn new(inner: Arc<dyn CredStoreClientV1>, system_ctx: Arc<SecurityContext>) -> Self {
        Self { inner, system_ctx }
    }

    /// Fetch a secret by reference.
    ///
    /// Returns `Ok(None)` if the secret does not exist or is inaccessible
    /// (matching the underlying `CredStoreClientV1::get` contract — access
    /// denial is expressed as `Ok(None)`, not as an error, to prevent
    /// enumeration). `Ok(Some(SecretString))` on success. Transport / auth
    /// failures bubble up as [`CredStoreError`].
    ///
    /// # Errors
    ///
    /// * Any infrastructure failure surfaced by the underlying client.
    /// * [`CredStoreError::Internal`] if the stored value is not valid UTF-8
    ///   (the plugin only stores UTF-8 client secrets — non-UTF-8 indicates
    ///   storage corruption or a key collision with another consumer).
    pub async fn get(&self, key: &SecretRef) -> Result<Option<SecretString>, CredStoreError> {
        let response = self.inner.get_secret(&self.system_ctx, key).await?;
        let Some(response) = response else {
            return Ok(None);
        };
        let value = std::str::from_utf8(response.value.as_bytes())
            .map_err(|e| CredStoreError::internal(format!("invalid UTF-8 in secret value: {e}")))?;
        Ok(Some(SecretString::from(value.to_owned())))
    }
}

#[cfg(test)]
#[path = "reader_tests.rs"]
mod reader_tests;
