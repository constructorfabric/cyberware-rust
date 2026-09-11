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
use crate::domain::secret::model::{Fallback, GcEntry, GcReason, NewSecret, SecretRow};

#[async_trait]
pub trait SecretRepo: Send + Sync {
    /// Resolve the winning row for `req_tenant` walking the ordered `chain`
    /// (req first, root last), applying two-phase priority + sharing. The
    /// resolution predicate is `status = active OR (status = declared AND
    /// fallback = none)` (ADR-0004, Suppression): a `declared`/`none` row
    /// competes and, when nearest, wins (blocking the walk); a
    /// `declared`/`inherit` row never competes.
    async fn resolve_for_get(
        &self,
        req_tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
        chain: &[Uuid],
    ) -> Result<Option<SecretRow>, DomainError>;

    /// Every row of the reference visible to the caller across `chain`
    /// (`req_tenant` first, root last), for the credential-**record** read
    /// (ADR-0004 `get`): the caller's own-tenant rows of **any** status
    /// (private-for-subject, tenant, shared — so the record view can report
    /// `declared`/`inherit` and its validator even though such a row never
    /// resolves), plus every ancestor's `shared` row that passes the
    /// resolution predicate above (an ancestor's `declared`/`inherit` row is
    /// invisible here, exactly as it is to a value read). One SQL query;
    /// [`crate::domain::secret::service::Service`] reduces the hierarchy in
    /// memory (ADR-0005 "Reducing a reference to one item": nearest
    /// resolvable row wins; a `declared`/`none` winner blocks).
    async fn resolve_candidates(
        &self,
        req_tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
        chain: &[Uuid],
    ) -> Result<Vec<SecretRow>, DomainError>;

    /// Find the caller's own-tenant row (two-phase: private-for-subject, else
    /// tenant/shared), of **either** resting status — a `declared` row is a
    /// legitimate "own record" `patch`/`delete` must be able to find.
    async fn find_own(
        &self,
        scope: &AccessScope,
        tenant: TenantId,
        subject: OwnerId,
        key: &SecretRef,
    ) -> Result<Option<SecretRow>, DomainError>;

    /// Find the row a write of `sharing` would target, by sharing-class identity
    /// (mirrors the partial unique indexes): `Private` → `(tenant, ref, owner)`,
    /// `Tenant`/`Shared` → `(tenant, ref)` among non-private — of **either**
    /// resting status, so `put` can see a `declared` row it must treat as
    /// "already exists" (ADR-0004: "does my tenant hold a row under this
    /// reference" counts a `declared` row too). Unlike [`Self::find_own`]
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

    // ── Collection read (ADR-0005) ──────────────────────────────────────────

    /// Step 1: candidate **references** visible across `chain`, under the
    /// same predicate [`Self::resolve_candidates`] applies per reference —
    /// own tenant: every sharing-visible row of any status; ancestors:
    /// resolution-eligible `shared` rows only — clamped by an exact
    /// `reference` or `secret_type_uuid` set when given (both invariant
    /// across a reference's chain). `DISTINCT reference`, ordered by
    /// `reference` (`desc` when `desc`), keyset-paginated by `cursor`
    /// (exclusive); fetches at most `limit` references. Never a `COUNT`.
    #[allow(
        clippy::too_many_arguments,
        reason = "every clamp the collection read's step 1 query supports"
    )]
    async fn list_candidate_references(
        &self,
        req_tenant: TenantId,
        subject: OwnerId,
        chain: &[Uuid],
        reference_in: Option<&[String]>,
        type_uuid_in: Option<&[Uuid]>,
        cursor: Option<&str>,
        desc: bool,
        limit: u64,
    ) -> Result<Vec<String>, DomainError>;

    /// The small second query of step 1 (ADR-0005): distinct
    /// `secret_type_uuid`s among the candidate rows of `references` — the
    /// same visibility predicate and `type_uuid_in` clamp
    /// [`Self::list_candidate_references`] applied, restricted to the
    /// references it found. This is what the collection read authorizes per
    /// type; a reduced winner whose type is outside this set (an
    /// override-type-consistency violation, since step 2 fetches whole rows
    /// unclamped by type) is dropped and counted, distinct from an ordinary
    /// PDP denial.
    async fn list_candidate_types(
        &self,
        req_tenant: TenantId,
        subject: OwnerId,
        chain: &[Uuid],
        references: &[String],
        type_uuid_in: Option<&[Uuid]>,
    ) -> Result<Vec<Uuid>, DomainError>;

    /// Step 2: every visible row of `references`, whole and unclamped by
    /// type — exactly what [`Self::resolve_candidates`] would return for
    /// each reference individually, so reduction sees every row a value
    /// read would see.
    async fn list_candidates_for_references(
        &self,
        req_tenant: TenantId,
        subject: OwnerId,
        chain: &[Uuid],
        references: &[String],
    ) -> Result<Vec<SecretRow>, DomainError>;

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
    /// `new.value_id` (with its fence stamp and `new.fallback`), and `DELETE`
    /// the matching `pending` gc entry (the intent is now realized). A
    /// unique-index conflict on the reference's own create-only uniqueness
    /// maps to the existing `Conflict` error; `new.value_id` is fresh, so it
    /// never collides with `uq_credstore_value_id` itself.
    async fn insert_active(&self, scope: &AccessScope, new: &NewSecret) -> Result<(), DomainError>;

    /// Overwrite step 4: ONE transaction —
    /// `UPDATE … SET value_id = new_value_id, value_fp, fp_key_id, sharing,
    /// fallback, expires_at, version = version + 1, updated_at = now(),
    /// status = active WHERE id = ? AND status IN (active, declared) [AND
    /// version = ?]`; 0 rows affected → `Ok(None)` (the caller maps this to
    /// `Conflict`/`VersionConflict`). Accepts a **`declared`** current row —
    /// `PUT`'s replace leg and `PATCH {"value": …}` both switch a `declared`
    /// row to `active` this way (ADR-0004, "Writing a value to a suppressed
    /// record is not a conflict"). On success, in the same transaction:
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
        fallback: Fallback,
        expires_at: Option<OffsetDateTime>,
        new_value_id: ValueId,
        value_fp: Vec<u8>,
        fp_key_id: i16,
    ) -> Result<Option<(SecretRow, Option<ValueId>)>, DomainError>;

    /// Metadata-only update (ADR-0004 `PATCH` with no `value` key): ONE
    /// transaction — `UPDATE … SET sharing, fallback, expires_at, version =
    /// version + 1, updated_at = now() WHERE id = ? [AND version = ?]`;
    /// never touches `value_id`/`value_fp`/`fp_key_id` or `status`. 0 rows
    /// affected → `Ok(None)` (version mismatch or the row vanished).
    async fn update_metadata(
        &self,
        scope: &AccessScope,
        id: Uuid,
        expected_version: Option<i64>,
        sharing: SharingMode,
        fallback: Fallback,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<Option<SecretRow>, DomainError>;

    /// Value-removal write (ADR-0004 `PATCH {"value": null}`, "How a record
    /// reaches it"): ONE transaction — `UPDATE … SET value_id = NULL, status
    /// = declared, value_fp = NULL, fp_key_id = NULL, sharing, fallback,
    /// expires_at, version = version + 1, updated_at = now() WHERE id = ?
    /// [AND version = ?]`, plus `INSERT gc(old_value_id, removed)` in the
    /// same transaction if the row held a value. 0 rows affected → `Ok(None)`.
    /// Returns the post-write (now `declared`) row plus the value id the
    /// caller best-effort deletes from the backend (`None` if the row was
    /// already `declared`, e.g. an idempotent re-send).
    async fn remove_value(
        &self,
        scope: &AccessScope,
        id: Uuid,
        expected_version: Option<i64>,
        sharing: SharingMode,
        fallback: Fallback,
        expires_at: Option<OffsetDateTime>,
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
