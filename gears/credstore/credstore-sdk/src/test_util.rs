//! Test-only [`CredStoreClientV1`] doubles, behind the `test-util` feature.
//!
//! A single configurable [`MockCredStoreClient`] covering the shapes consumers
//! exercise in tests, so each gear no longer hand-rolls its own:
//!
//! * [`MockCredStoreClient::empty`] — every `get`/`get_secret` resolves to
//!   `Ok(None)`;
//! * [`MockCredStoreClient::with_secrets`] — a keyed `(reference, value)` store;
//! * [`MockCredStoreClient::returning_raw_value`] — a fixed raw value for any
//!   reference (e.g. non-UTF-8 bytes to drive malformed-value paths);
//! * [`MockCredStoreClient::always_failing`] — every operation fails with
//!   [`CredStoreError::Internal`].
//!
//! Only `get`/`get_secret`/`list` carry behaviour; the write half (`put`/
//! `patch`/`delete`) is a no-op that succeeds (or fails, in the
//! always-failing mode) to match, returning a placeholder validator — this
//! double is read-oriented, for consumers that only resolve credentials.
//! `list` returns every stored reference as one unfiltered, unpaginated item
//! (it does not model the `OData` allowlist or cursor semantics a real server
//! enforces); [`MockCredStoreClient`] also implements
//! [`crate::CredStoreMaintenanceV1`], whose `run_gc` mirrors the write
//! half's success/failure behaviour and reports an all-zero
//! [`crate::GcReport`].

use std::collections::HashMap;

use async_trait::async_trait;
use toolkit_odata::{ODataQuery, Page, PageInfo};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::{
    CredStoreClientV1, CredStoreError, CredStoreMaintenanceV1, Credential, CredentialListItem,
    CredentialPatch, CredentialStatus, CredentialWrite, Fallback, GcReport, InheritanceStatus,
    OwnerId, PutOutcome, PutPrecondition, Secret, SecretRef, SecretType, SecretValue, SharingMode,
    Validator, WritePrecondition,
};

enum Behavior {
    /// `get`/`get_secret` return the mapped value for a known reference, else
    /// `Ok(None)`.
    Store(HashMap<String, Vec<u8>>),
    /// `get`/`get_secret` return this raw value for *any* reference.
    AnyValue(Vec<u8>),
    /// Every operation fails with [`CredStoreError::Internal`].
    Failing,
    /// `get`/`get_secret` fail with [`CredStoreError::NotFound`] — a client
    /// implementation that reports the not-found surface as an error instead
    /// of `Ok(None)`.
    NotFound,
}

/// Configurable in-process [`CredStoreClientV1`] test double. See the module
/// docs for the available modes.
pub struct MockCredStoreClient {
    behavior: Behavior,
}

impl MockCredStoreClient {
    /// Empty store — every `get`/`get_secret` resolves to `Ok(None)`.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            behavior: Behavior::Store(HashMap::new()),
        }
    }

    /// Store seeded with `(reference, value)` pairs; an unknown reference
    /// resolves to `Ok(None)`. A leading `cred://` scheme prefix on a key is
    /// stripped, so callers using either the bare reference or the consumer's
    /// `cred://<ref>` spelling resolve identically.
    #[must_use]
    pub fn with_secrets(creds: Vec<(String, String)>) -> Self {
        let store = creds
            .into_iter()
            .map(|(k, v)| {
                let key = k.strip_prefix("cred://").unwrap_or(&k).to_owned();
                (key, v.into_bytes())
            })
            .collect();
        Self {
            behavior: Behavior::Store(store),
        }
    }

    /// Resolve *any* reference to this raw value — useful for exercising
    /// non-UTF-8 / malformed-value paths.
    #[must_use]
    pub fn returning_raw_value(value: Vec<u8>) -> Self {
        Self {
            behavior: Behavior::AnyValue(value),
        }
    }

    /// Every operation fails with [`CredStoreError::Internal`] — for
    /// error-handling paths.
    #[must_use]
    pub fn always_failing() -> Self {
        Self {
            behavior: Behavior::Failing,
        }
    }

    /// `get`/`get_secret` fail with [`CredStoreError::NotFound`] for any
    /// reference — for consumers hardening against clients that report the
    /// not-found surface as an error instead of `Ok(None)`.
    #[must_use]
    pub fn erroring_not_found() -> Self {
        Self {
            behavior: Behavior::NotFound,
        }
    }

    /// Build a canned [`Credential`] record with placeholder metadata (nil
    /// generation id, `generic` type, version 1, own/active, no expiry).
    fn credential(reference: &SecretRef) -> Credential {
        Credential {
            reference: reference.clone(),
            secret_type: SecretType::generic().gts_id().to_owned(),
            sharing: SharingMode::default(),
            fallback: Some(Fallback::default()),
            status: CredentialStatus::Active,
            inheritance: InheritanceStatus::Own,
            version: Some(1),
            updated_at: None,
            owner_id: Some(OwnerId::nil()),
            expires_at: None,
            validator: Some(Self::validator()),
        }
    }

    /// Build a canned [`Secret`] wrapping `value` with placeholder metadata.
    fn secret(reference: &SecretRef, value: Vec<u8>) -> Secret {
        Secret {
            reference: reference.clone(),
            secret_type: SecretType::generic().gts_id().to_owned(),
            expires_at: None,
            value: SecretValue::new(value),
            validator: Self::validator(),
        }
    }

    fn validator() -> Validator {
        Validator {
            id: Uuid::nil(),
            version: 1,
        }
    }

    fn write_result(&self) -> Result<(), CredStoreError> {
        match self.behavior {
            Behavior::Failing => Err(CredStoreError::Internal("backend failure".into())),
            Behavior::Store(_) | Behavior::AnyValue(_) | Behavior::NotFound => Ok(()),
        }
    }
}

#[async_trait]
impl CredStoreClientV1 for MockCredStoreClient {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<Credential>, CredStoreError> {
        match &self.behavior {
            Behavior::Store(store) => Ok(store
                .contains_key(key.as_ref())
                .then(|| Self::credential(key))),
            Behavior::AnyValue(_) => Ok(Some(Self::credential(key))),
            Behavior::Failing => Err(CredStoreError::Internal("backend failure".into())),
            Behavior::NotFound => Err(CredStoreError::NotFound),
        }
    }

    async fn get_secret(
        &self,
        _ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<Secret>, CredStoreError> {
        match &self.behavior {
            Behavior::Store(store) => Ok(store
                .get(key.as_ref())
                .cloned()
                .map(|v| Self::secret(key, v))),
            Behavior::AnyValue(value) => Ok(Some(Self::secret(key, value.clone()))),
            Behavior::Failing => Err(CredStoreError::Internal("backend failure".into())),
            Behavior::NotFound => Err(CredStoreError::NotFound),
        }
    }

    async fn put(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
        _write: CredentialWrite,
        precondition: PutPrecondition,
    ) -> Result<PutOutcome, CredStoreError> {
        self.write_result().map(|()| PutOutcome {
            created: matches!(precondition, PutPrecondition::CreateOnly),
            validator: Self::validator(),
        })
    }

    async fn patch(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
        _patch: CredentialPatch,
        _precondition: WritePrecondition,
    ) -> Result<Validator, CredStoreError> {
        self.write_result().map(|()| Self::validator())
    }

    async fn delete(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
        _precondition: WritePrecondition,
    ) -> Result<(), CredStoreError> {
        self.write_result()
    }

    /// Minimal double: every stored reference is returned as one item
    /// (unfiltered, unpaginated — this test double is read-oriented and does
    /// not model the `OData` allowlist, reduction, or cursor semantics a real
    /// server enforces). `secret` is populated only when `query`'s `$select`
    /// names it, mirroring the real value-mode switch.
    async fn list(
        &self,
        _ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<CredentialListItem>, CredStoreError> {
        let limit = query.limit.unwrap_or(50);
        let value_mode = query
            .selected_fields()
            .is_some_and(|fields| fields.iter().any(|f| f.eq_ignore_ascii_case("secret")));
        match &self.behavior {
            Behavior::Failing => Err(CredStoreError::Internal("backend failure".into())),
            Behavior::NotFound | Behavior::AnyValue(_) => Ok(Page::empty(limit)),
            Behavior::Store(store) => {
                let mut items: Vec<CredentialListItem> = store
                    .iter()
                    .filter_map(|(k, v)| {
                        let key = SecretRef::new(k.clone()).ok()?;
                        Some(CredentialListItem {
                            credential: Self::credential(&key),
                            secret: value_mode.then(|| SecretValue::new(v.clone())),
                        })
                    })
                    .collect();
                items.sort_by(|a, b| {
                    a.credential
                        .reference
                        .as_ref()
                        .cmp(b.credential.reference.as_ref())
                });
                Ok(Page::new(
                    items,
                    PageInfo {
                        next_cursor: None,
                        prev_cursor: None,
                        limit,
                    },
                ))
            }
        }
    }
}

#[async_trait]
impl CredStoreMaintenanceV1 for MockCredStoreClient {
    async fn run_gc(&self, _ctx: &SecurityContext) -> Result<GcReport, CredStoreError> {
        self.write_result().map(|()| GcReport::default())
    }
}
