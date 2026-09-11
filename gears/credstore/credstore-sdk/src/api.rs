//! Consumer contract for tenant-scoped credential operations (ADR-0004: the
//! credential surface).
//!
//! Defines anti-enumerating reads and explicit optimistic-concurrency
//! semantics for the record and its value as two separate representations —
//! see [`crate::models::Credential`] and [`crate::models::Secret`].

use async_trait::async_trait;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::SecurityContext;

use crate::error::CredStoreError;
use crate::models::{
    Credential, CredentialListItem, CredentialPatch, CredentialWrite, PutOutcome, PutPrecondition,
    Secret, SecretRef, Validator, WritePrecondition,
};

/// Consumer-facing API trait for credential storage operations. Seven
/// methods, none named `create` or `read_secrets`: `put` under
/// [`PutPrecondition::CreateOnly`] **is** create, and [`Self::list`] is the
/// collection read (metadata by default; `$select` containing `secret`
/// switches it to bulk value mode, ADR-0005/ADR-0004).
#[async_trait]
pub trait CredStoreClientV1: Send + Sync {
    /// Retrieves the credential **record** by reference, applying
    /// hierarchical resolution. Never carries the value — see
    /// [`Self::get_secret`].
    ///
    /// Returns `Ok(Some(_))` for an accessible record — including one whose
    /// caller-tenant row is `declared` (no value) — `Ok(None)` when the
    /// reference resolves to nothing the caller may see (a single 404
    /// surface that prevents enumeration), and `Err(AccessDenied)` only when
    /// the PDP evaluation itself cannot be completed.
    ///
    /// Requires the `read` action.
    async fn get(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<Credential>, CredStoreError>;

    /// Retrieves the resolved **value**, applying hierarchical resolution. A
    /// winning record with no value (`declared`, or `suppressed`) is the
    /// canonical miss — `Ok(None)`, identical to "does not exist".
    ///
    /// Requires the `read_secret` action.
    async fn get_secret(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<Secret>, CredStoreError>;

    /// Creates or replaces the whole credential — record and value together,
    /// in one call. `precondition` carries the intent:
    /// [`PutPrecondition::CreateOnly`] fails with [`CredStoreError::Conflict`]
    /// if the caller's own tenant already holds a record under the
    /// reference; [`PutPrecondition::Exists`] / [`PutPrecondition::Matches`]
    /// fail the same way if it does not (a `put` under either never
    /// creates).
    ///
    /// Requires **both** `write` and `write_secret`, evaluated before any
    /// side effect — a caller missing either fails the whole request.
    ///
    /// # Errors
    ///
    /// Returns [`CredStoreError::Conflict`] on a failed precondition (create
    /// found an existing row, or replace found none) or a lost CAS.
    /// Returns [`CredStoreError::TypeViolation`] on a trait violation, an
    /// unresolvable type, or an attempted type change (`TYPE_IMMUTABLE`) —
    /// including creating over a reference that currently resolves to an
    /// ancestor's `shared` record of a different type
    /// (`TYPE_MISMATCH_WITH_INHERITED`).
    async fn put(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        write: CredentialWrite,
        precondition: PutPrecondition,
    ) -> Result<PutOutcome, CredStoreError>;

    /// Applies a partial change to the record, the value, or both — RFC 7396
    /// JSON Merge Patch semantics: a field present is applied exactly as
    /// [`Self::put`] would apply it, a field absent is left untouched.
    /// `patch.value` present as [`crate::models::PatchField::Null`] removes
    /// the value (the record becomes `declared`); as
    /// [`crate::models::PatchField::Set`] it rotates/creates it.
    ///
    /// The action set is derived from the body: any metadata field present
    /// (`sharing`/`fallback`/`secret_type`/`expires_at`) requires `write`;
    /// `value` present (`Set` or `Null`) requires `write_secret`; both
    /// present require both — all required actions are evaluated before any
    /// side effect. Never creates: no own record under the reference is
    /// [`CredStoreError::NotFound`].
    ///
    /// A patch whose metadata equals the current record and carries no
    /// `value` key is a no-op: it returns the current validator unchanged,
    /// without bumping the version.
    ///
    /// # Errors
    ///
    /// Returns [`CredStoreError::NotFound`] if the caller holds no own
    /// record under the reference.
    /// Returns [`CredStoreError::Conflict`] on a failed precondition.
    /// Returns [`CredStoreError::TypeViolation`] on an empty patch
    /// (`EMPTY_PATCH`), a trait violation, or a `secret_type` differing from
    /// the stored one (`TYPE_IMMUTABLE`).
    async fn patch(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        patch: CredentialPatch,
        precondition: WritePrecondition,
    ) -> Result<Validator, CredStoreError>;

    /// Deletes the caller's own-tenant credential (record and value
    /// together), guarded by the mandatory `precondition`. Releases the
    /// reference at once.
    ///
    /// Requires the `delete` action.
    ///
    /// # Errors
    ///
    /// Returns [`CredStoreError::NotFound`] if no own-tenant record exists.
    /// Returns [`CredStoreError::Conflict`] on a failed precondition.
    async fn delete(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
        precondition: WritePrecondition,
    ) -> Result<(), CredStoreError>;

    /// Lists the credentials visible to the caller's tenant, rooted at that
    /// tenant and walking upward through its ancestor chain only — never
    /// downward (ADR-0005, "Upward-rooted collection read"). One item per
    /// reference: the same reduction a point read (`Self::get`) of that
    /// reference would apply, so the catalogue and the point read never
    /// disagree.
    ///
    /// `query.filter()` accepts `reference` and `type` (SQL-clamped;
    /// `eq`/`in` only) plus `sharing`, `fallback` and `expires_at` (applied
    /// after reduction, since they vary across a reference's chain);
    /// `inheritance`, `owner_tenant_id` and `updated_at` are never
    /// filterable or orderable. `query.order` accepts only `reference`
    /// (ascending by default). `query.selected_fields()` accepts the
    /// `Credential` field names plus `secret`.
    ///
    /// Selecting `secret` switches the request to **value mode**: `limit`
    /// and `query.cursor` are rejected, `query.order` must be empty, the
    /// selector in `query.filter()` must be exactly `reference` or `type`
    /// (`eq`/`in`), the match set is capped, and each returned item's
    /// [`CredentialListItem::secret`] carries the decrypted value for the
    /// items the caller may read — a refused, missing, or
    /// fingerprint-mismatched item is omitted rather than reported.
    /// `Page::page_info.next_cursor` is always `None` in this mode; there is
    /// no pagination over a value-mode match set.
    ///
    /// A caller whose scope does not admit its own tenant gets an empty page,
    /// never [`CredStoreError::AccessDenied`] (ADR-0005: the PDP resource is
    /// the resolved concrete type, so there is nothing to evaluate — and
    /// therefore nothing to deny — until rows exist).
    ///
    /// Requires the `list` action per distinct type present among candidates
    /// (metadata mode) or `read_secret` (value mode); a type the caller may
    /// not read is dropped from the page rather than failing the request.
    ///
    /// # Errors
    ///
    /// Returns [`CredStoreError::InvalidRequest`] if `query` names an
    /// unsupported filter/order field, an out-of-range `limit`, a malformed
    /// cursor, a cursor minted under a different filter/order, or a
    /// value-mode request that also carries pagination — or, in value mode,
    /// if the selector is not `reference`/`type` `eq`/`in`, or the selector
    /// matches more than the configured cap.
    async fn list(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<CredentialListItem>, CredStoreError>;
}
