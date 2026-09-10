//! Backend storage-plugin contract (ADR-0006: immutable value versions).
//!
//! A plugin is a dumb per-tenant key-value store keyed by `tenant_id/value_id`
//! — no `reference`, no sharing-derived key class, no `owner_id`. Every entry,
//! once written, is immutable: the gear always mints a fresh
//! [`ValueId`](crate::models::ValueId) before it writes, so a `put` to a
//! `value_id` the gear has already written is a contract violation the gear
//! itself never issues; a plugin MAY defend against it anyway by rejecting
//! such a call with [`CredStoreError::Conflict`]. `delete` of a `value_id` the
//! plugin does not (or no longer) hold is success (idempotent) — the gear's
//! garbage-collection drain and its best-effort post-write cleanup both rely
//! on a duplicate delete being harmless. The plugin learns nothing about
//! references, owners, or sharing: `value_id` is unique across the whole
//! store, so the metadata row alone knows which value belongs to which
//! reference and which sharing class; the backend needs neither to do its
//! job.

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::error::CredStoreError;
use crate::models::{SecretValue, TenantId, ValueId};

/// Pure per-tenant, per-version value store. See the module docs.
#[async_trait]
pub trait CredStorePluginClientV1: Send + Sync {
    /// Retrieves the value stored at `(tenant_id, value_id)`, or `None` when
    /// no entry exists there (never written, or already collected).
    async fn get(
        &self,
        ctx: &SecurityContext,
        tenant_id: &TenantId,
        value_id: &ValueId,
    ) -> Result<Option<SecretValue>, CredStoreError>;

    /// Writes a brand-new, immutable entry at `(tenant_id, value_id)`. The
    /// gear never reuses a `value_id` for a second `put`; a plugin MAY reject
    /// a `put` to an id it already holds with [`CredStoreError::Conflict`] as
    /// a defensive backstop against a contract violation.
    async fn put(
        &self,
        ctx: &SecurityContext,
        tenant_id: &TenantId,
        value_id: &ValueId,
        value: SecretValue,
    ) -> Result<(), CredStoreError>;

    /// Deletes the entry at `(tenant_id, value_id)`. Deleting an id the
    /// plugin does not hold is success (idempotent) — callers (the gear's
    /// post-write cleanup and its garbage-collection drain) rely on this.
    async fn delete(
        &self,
        ctx: &SecurityContext,
        tenant_id: &TenantId,
        value_id: &ValueId,
    ) -> Result<(), CredStoreError>;
}
