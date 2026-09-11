//! Backend storage-plugin contract (ADR-0006: immutable value versions).
//!
//! A plugin is a dumb per-tenant key-value store keyed by `tenant_id/value_id`
//! — no `reference`, no sharing-derived key class, no `owner_id`. Every entry,
//! once written, is immutable: the gear always mints a fresh
//! [`ValueId`](crate::models::ValueId) before it writes, so a `put` to a
//! `value_id` the gear has already written is a contract violation the gear
//! itself never issues; a plugin **MUST** reject such a call with
//! [`CredStoreError::Conflict`] rather than silently overwrite — this is not
//! merely a defensive backstop: the fence-key bootstrap (`Service::
//! load_fence_key`) relies on a losing replica's `put` to the fixed fence-key
//! `value_id` being rejected this way to detect "another replica already won"
//! and safely re-read instead of clobbering the winner's key. `delete` of a `value_id` the
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
    /// gear never reuses a `value_id` for a second `put`, but a conforming
    /// plugin **MUST** still reject a `put` to an id it already holds with
    /// [`CredStoreError::Conflict`] — immutability is a contract requirement,
    /// not an optional defensive backstop, since the fence-key bootstrap
    /// relies on this rejection across replicas to tell "another replica
    /// already won" apart from an actual failure.
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
