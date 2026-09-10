//! Persistence port for secret metadata, value-version switching, and
//! garbage-collection bookkeeping (ADR-0006: immutable value versions).
//!
//! Every method that changes more than one row/table runs as ONE database
//! transaction — that is not negotiable: [`Self::insert_active`],
//! [`Self::switch_value`], [`Self::delete_by_id`], and
//! [`Self::delete_expired_row`] each pair a `credstore_secrets` mutation with
//! its `credstore_value_gc` bookkeeping atomically.

use async_trait::async_trait;
use credstore_sdk::{OwnerId, SecretRef, SharingMode, TenantId, ValueId};
use time::OffsetDateTime;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::secret::model::{GcEntry, GcReason, NewSecret, SecretRow};

#[async_trait]
pub trait SecretRepo: Send + Sync {
    /// Resolve the winning active secret for `req_tenant` walking the ordered
    /// `chain` (req first, root last), applying two-phase priority + sharing.
    /// The predicate stays `status = active` in Phase 1 — a `declared` row is
    /// never a resolution candidate yet (ADR-0004's fallback competition is
    /// out of scope here).
    async fn resolve_for_get(
        &self,
        req_tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
        chain: &[Uuid],
    ) -> Result<Option<SecretRow>, DomainError>;

    /// Find the caller's own-tenant row (two-phase: private-for-subject, else
    /// tenant/shared).
    async fn find_own(
        &self,
        scope: &AccessScope,
        tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
    ) -> Result<Option<SecretRow>, DomainError>;

    /// Find the row a write of `sharing` would target, by sharing-class identity
    /// (mirrors the partial unique indexes): `Private` → `(tenant, ref, owner)`,
    /// `Tenant`/`Shared` → `(tenant, ref)` among non-private. Unlike [`Self::find_own`]
    /// this never crosses the private boundary, so a private write does not see a
    /// coexisting tenant/shared secret (and vice-versa) — they coexist per design.
    async fn find_for_write(
        &self,
        scope: &AccessScope,
        tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
        sharing: SharingMode,
    ) -> Result<Option<SecretRow>, DomainError>;

    /// True iff `tenant` is within the read `scope` (closure-backed for subtree).
    async fn scope_includes_tenant(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
    ) -> Result<bool, DomainError>;

    // ── Garbage-collection bookkeeping (`credstore_value_gc`) ───────────────

    /// Record a write's intent: `INSERT credstore_value_gc(value_id, tenant_id,
    /// reason = pending)` — the durable trace that bytes were about to be
    /// written, before the backend call. Unscoped: the gc table carries no
    /// PDP-relevant data (§4.7's data-domain note).
    async fn gc_insert_pending(
        &self,
        value_id: ValueId,
        tenant_id: TenantId,
    ) -> Result<(), DomainError>;

    /// Delete a gc entry by id. Returns whether a row was removed (a
    /// best-effort caller treats either outcome as done).
    async fn gc_delete(&self, value_id: ValueId) -> Result<bool, DomainError>;

    /// Mark an existing gc entry's `reason` (e.g. a CAS loser's version →
    /// `Aborted`). Returns `false` if no entry exists for `value_id`.
    async fn gc_mark(&self, value_id: ValueId, reason: GcReason) -> Result<bool, DomainError>;

    /// Up to `limit` gc entries ordered by `enqueued_at` (oldest first) —
    /// the maintenance job's batch source for both its gc-drain and
    /// pending-reclaim passes.
    async fn gc_list(&self, limit: u64) -> Result<Vec<GcEntry>, DomainError>;

    /// `EXISTS` — via `SELECT … LIMIT 1`, never `COUNT` (platform rule) —
    /// whether any row currently points at `value_id`. The maintenance job's
    /// pending-reclaim pass uses this as its defensive backstop before ever
    /// deleting a `pending` entry's backend bytes.
    async fn is_value_referenced(&self, value_id: ValueId) -> Result<bool, DomainError>;

    // ── Write protocol (ADR-0006 §6.2) ──────────────────────────────────────

    /// Create step 4: ONE transaction — `INSERT` the row `active` pointing at
    /// `new.value_id` (with its fence stamp), and `DELETE` the matching
    /// `pending` gc entry (the intent is now realized). A unique-index
    /// conflict on the reference's own create-only uniqueness maps to the
    /// existing `Conflict` error; `new.value_id` is fresh, so it never
    /// collides with `uq_credstore_value_id` itself.
    async fn insert_active(&self, scope: &AccessScope, new: &NewSecret) -> Result<(), DomainError>;

    /// Overwrite step 4: ONE transaction —
    /// `UPDATE … SET value_id = new_value_id, value_fp, fp_key_id, sharing,
    /// expires_at, version = version + 1, updated_at = now(), status = active
    /// WHERE id = ? [AND version = ?]`; 0 rows affected → `Ok(None)` (the
    /// caller maps this to `Conflict`). On success, in the same transaction:
    /// `DELETE` the `pending` entry for `new_value_id`, and — reading the
    /// row's previous `value_id` inside the transaction (locked, so a
    /// concurrent switch can't race the read) — if it was not `None`,
    /// `INSERT` a `superseded` gc entry for it. Returns the post-write row
    /// plus the previous `value_id` (for the caller's best-effort backend
    /// cleanup).
    #[allow(
        clippy::too_many_arguments,
        reason = "one CAS with every field it may update"
    )]
    async fn switch_value(
        &self,
        scope: &AccessScope,
        id: Uuid,
        expected_version: Option<i64>,
        sharing: SharingMode,
        expires_at: Option<OffsetDateTime>,
        new_value_id: ValueId,
        value_fp: Vec<u8>,
        fp_key_id: i16,
    ) -> Result<Option<(SecretRow, Option<ValueId>)>, DomainError>;

    /// Delete step 2: ONE transaction — `DELETE` the row (0 rows affected →
    /// the existing not-found/conflict semantics, i.e. `DomainError::NotFound`)
    /// and, if it held a value, `INSERT` a `removed` gc entry for it. Returns
    /// the row's `value_id` (`None` if it was already `declared`) for the
    /// caller's best-effort backend cleanup.
    async fn delete_by_id(
        &self,
        scope: &AccessScope,
        id: Uuid,
        expected_version: Option<i64>,
    ) -> Result<Option<ValueId>, DomainError>;

    // ── Maintenance job support (ADR-0006 §6.4) ─────────────────────────────

    /// Up to `limit` `active` rows past their `expires_at` (unscoped — the
    /// job runs without a caller).
    async fn list_expired(&self, limit: u64) -> Result<Vec<SecretRow>, DomainError>;

    /// The same one-transaction delete as [`Self::delete_by_id`] (row delete +
    /// `removed` gc insert), unscoped and without a version gate — the
    /// maintenance job owns the row outright once it is past `expires_at`.
    async fn delete_expired_row(&self, id: Uuid) -> Result<Option<ValueId>, DomainError>;
}
