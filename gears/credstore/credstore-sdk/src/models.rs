// Updated: 2026-04-07 by Constructor Tech
// Updated: 2026-03-18 by Constructor Tech
//! Public credential references, values, metadata, sharing, expiry, and write
//! preconditions.
//!
//! Secret values redact formatting output and zeroize their bytes on drop.
use std::fmt;

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;
use zeroize::Zeroize;

use gts::GtsId;

use crate::error::CredStoreError;

/// Re-export from tenant-resolver-sdk for cross-gear type consistency.
pub use tenant_resolver_sdk::TenantId;

/// Owner identifier, representing `SecurityContext.subject_id()`. A row-level
/// access key only — it plays no part in the backend key shape (ADR-0006):
/// the plugin never sees it, and a private row's ownership is enforced by
/// the metadata row alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OwnerId(pub Uuid);

/// Opaque backend-value identifier (ADR-0006 "immutable value versions").
///
/// Every value write mints a fresh `ValueId` (a UUID v4) and writes the bytes
/// to the backend under `(tenant_id, value_id)` — never `reference` or a
/// sharing-derived key class. A `credstore_secrets` row's `value_id` column
/// points at the version it currently serves; `value_id` is unique across the
/// whole store, so the row alone knows which value belongs to which
/// reference, and the backend needs neither `reference` nor `owner_id` to do
/// its job. See [`crate::plugin_api`] and
/// [`FENCE_KEY_VALUE_ID`](crate::types::FENCE_KEY_VALUE_ID).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ValueId(pub Uuid);

impl ValueId {
    /// Mint a fresh, store-wide-unique value id for a new write.
    #[must_use]
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Display for ValueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl OwnerId {
    /// Returns the nil UUID wrapped as an `OwnerId`.
    #[must_use]
    pub fn nil() -> Self {
        Self(Uuid::nil())
    }

    /// Returns `true` if the inner UUID is the nil UUID.
    #[must_use]
    pub fn is_nil(&self) -> bool {
        self.0.is_nil()
    }
}

impl fmt::Display for OwnerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// A validated secret reference key.
///
/// Format: `[a-zA-Z0-9_-]+`, max 255 characters.
/// Colons are prohibited to prevent `ExternalID` collisions in backend storage.
#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
pub struct SecretRef(String);

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        SecretRef::new(s).map_err(serde::de::Error::custom)
    }
}

impl SecretRef {
    /// Creates a new `SecretRef` after validating the format.
    ///
    /// # Errors
    ///
    /// Returns `CredStoreError::InvalidSecretRef` if the input is empty,
    /// exceeds 255 characters, or contains characters outside `[a-zA-Z0-9_-]`.
    #[must_use = "returns a Result that may contain a validation error"]
    pub fn new(value: impl Into<String>) -> Result<Self, CredStoreError> {
        let value = value.into();
        if value.is_empty() {
            return Err(CredStoreError::invalid_ref("must not be empty"));
        }
        if value.len() > 255 {
            return Err(CredStoreError::invalid_ref(
                "exceeds maximum length of 255 characters",
            ));
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(CredStoreError::invalid_ref(
                "contains invalid characters; only [a-zA-Z0-9_-] are allowed",
            ));
        }
        Ok(Self(value))
    }
}

impl AsRef<str> for SecretRef {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SecretRef").field(&self.0).finish()
    }
}

/// A secret value with redacted Debug/Display output.
///
/// Wraps opaque bytes (`Vec<u8>`) and guarantees that content is never
/// leaked through formatting. Does not implement `Serialize`/`Deserialize`
/// to prevent accidental serialization of secret data.
pub struct SecretValue(Vec<u8>);

impl SecretValue {
    /// Creates a new `SecretValue` from raw bytes.
    #[must_use]
    pub fn new(value: Vec<u8>) -> Self {
        Self(value)
    }

    /// Returns a reference to the raw bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for SecretValue {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl From<String> for SecretValue {
    fn from(value: String) -> Self {
        Self(value.into_bytes())
    }
}

impl From<&str> for SecretValue {
    fn from(value: &str) -> Self {
        Self(value.as_bytes().to_vec())
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl fmt::Display for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Controls the visibility scope of a stored secret.
///
/// Also part of the GTS trait vocabulary: `SecretTypeTraits::allow_sharing`
/// lists the modes a secret type permits, so the derived `x-gts-traits-schema`
/// constrains trait values to this enum (schemars follows the serde
/// `snake_case` renames).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Only the owner can access the secret.
    Private,
    /// All users within the owner's tenant can access the secret.
    #[default]
    Tenant,
    /// The secret is accessible across tenant boundaries.
    Shared,
}

/// Optimistic-concurrency precondition for `patch`/`delete` — the in-process
/// equivalent of the REST `If-Match` header, and a **required** argument of
/// every update/delete: there are no unconditional overwrites. Read the
/// current generation from a prior [`Credential`]/[`Secret`] validator
/// (`id` + `version`) and send [`Self::Matches`]; a failed precondition
/// surfaces as [`CredStoreError::Conflict`].
///
/// [`Self::Exists`] is the deliberate, visible-in-code opt-out for blind
/// create-or-replace flows that cannot hold a version (rotation /
/// provisioning, where the new value is not derived from the stored one).
/// It is also what a caller recovering a fence-poisoned reference (ADR-0003)
/// uses, since a fenced `GET` fails closed with 404 and yields no validator
/// to hold — but under immutable value versions (ADR-0006) that recovery is
/// an ordinary new write like any other, not a special healing path;
/// `Exists` carries no meaning beyond RFC 9110 last-writer-wins.
///
/// `put` uses the distinct [`PutPrecondition`] instead, which additionally
/// carries the create-only intent (`If-None-Match: *`, ADR-0004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePrecondition {
    /// The target credential must already exist (REST `If-Match: *`). This is
    /// an explicit last-writer-wins overwrite: concurrent `Exists` writers to
    /// one reference race and the later write survives wholesale — each lands
    /// under its own immutable value version (ADR-0006), so the race
    /// resolves into two intact versions and one pointer, never a corrupted
    /// value. Reserve it for writers that own their references outright
    /// (rotation, provisioning) and for a caller with no version to hold (a
    /// fenced `GET` fails closed with no validator to read); read-modify-write
    /// callers must use [`Self::Matches`].
    Exists,
    /// Compare-and-set: the current generation must still be `(id, version)`
    /// (REST `If-Match: "<id>.<version>"`). `id` is the row UUID — fresh per
    /// recreated credential — so a validator from an earlier generation never
    /// matches after a delete/recreate even if the version counters coincide.
    Matches {
        /// Row (generation) UUID from the observed [`Validator::id`].
        id: uuid::Uuid,
        /// Version counter from the observed [`Validator::version`].
        version: i64,
    },
}

/// The strong `ETag` source (ADR-0004, D4): a credential row's generation id
/// plus its per-generation monotonic version. Renders on the wire as
/// `"<id>.<version>"`. `Credential::validator` is `None` exactly when the
/// caller's tenant holds no row under the reference, in which case the REST
/// layer serves a weak, opaque validator instead (derived from the winning
/// row, never usable in `If-Match`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validator {
    /// Row (generation) UUID, minted fresh for every recreated credential.
    pub id: Uuid,
    /// Monotonic version counter within this generation.
    pub version: i64,
}

/// Outcome of [`CredStoreClientV1::put`](crate::CredStoreClientV1::put):
/// whether the call created the record (`If-None-Match: *`, 201) or replaced
/// it (204), and the validator to hand back as the response `ETag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutOutcome {
    /// `true` for a create (`If-None-Match: *`), `false` for a replace.
    pub created: bool,
    /// The written row's fresh validator.
    pub validator: Validator,
}

/// Precondition for [`CredStoreClientV1::put`](crate::CredStoreClientV1::put)
/// (ADR-0004, "Two write verbs on one resource"). Unlike [`WritePrecondition`],
/// `put` distinguishes a create-only intent from a guarded or unconditional
/// replace, because `PUT` is the one address that can both create and
/// replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutPrecondition {
    /// `If-None-Match: *` — create-only; `Conflict` if the caller's own
    /// tenant already holds a record under the reference (an inherited
    /// representation does not count, ADR-0004 "If it exists is judged
    /// against the caller's own tenant").
    CreateOnly,
    /// `If-Match: *` — replace, last-writer-wins; `Conflict` if no own record
    /// exists (a `PUT` never creates under this precondition).
    Exists,
    /// `If-Match: "<id>.<version>"` — guarded replace.
    Matches(Validator),
}

/// Suppression policy for the caller's own record (ADR-0004, "Suppression"):
/// what a reference means while its record holds no value. Shown only for
/// the caller's own row — an ancestor's policy is not the caller's to see —
/// and is present in [`CredentialWrite`]/[`CredentialPatch`] under the
/// `write` action, the same action that governs `sharing`.
///
/// Wire form: lowercase `"inherit"` / `"none"`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Fallback {
    /// While the record holds no value, resolution keeps walking up the
    /// tenant chain past it (default).
    #[default]
    Inherit,
    /// While the record holds no value, it blocks resolution outright when
    /// it is the nearest candidate (suppression).
    None,
}

/// The state of the caller's **own row** under a reference (ADR-0004, "Two
/// representations"): never a saga state, never `provisioning` or
/// `deprovisioning` — those are invisible to every read.
///
/// Wire form: lowercase `"none"` / `"declared"` / `"active"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    /// The caller's tenant holds no row under the reference at all.
    None,
    /// The caller's own row exists but carries no value
    /// (`PATCH {"value": null}` was the last write to touch it).
    Declared,
    /// The caller's own row exists and carries a value.
    Active,
}

/// The state of the **effective row** a reference resolves to (ADR-0004,
/// "Two representations"), which need not be the caller's own row. Computed
/// at resolution/reduction time, never a stored or filterable column.
///
/// Wire form: lowercase `"own"` / `"inherited"` / `"overridden"` /
/// `"suppressed"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InheritanceStatus {
    /// The winning row is the caller's own, and no ancestor's `shared` row
    /// under the same reference is in play.
    Own,
    /// The winning row is an ancestor's `shared` row.
    Inherited,
    /// The caller's own row shadows an ancestor's `shared` row under the same
    /// reference.
    Overridden,
    /// The winning row is a value-less record with `fallback: none` — the
    /// walk stops there rather than falling through (own or an ancestor's).
    Suppressed,
}

/// The addressable **credential record** (ADR-0004, "Two representations"):
/// reference, type, sharing, and two independent status fields — never the
/// value, and never the owning tenant. `GET /credentials/{ref}`'s response
/// shape and the collection item's shape (Phase 3).
#[derive(Debug, Clone, PartialEq)]
pub struct Credential {
    /// The caller-chosen reference this record answers to.
    pub reference: SecretRef,
    /// The resolved record's full GTS type id (the effective record's type —
    /// the caller's own row's type if it has one, else the winner's).
    pub secret_type: String,
    /// Sharing mode of the effective record.
    pub sharing: SharingMode,
    /// Suppression policy of the caller's **own** row; `None` when the
    /// caller's tenant holds no row under the reference (`status: none`).
    pub fallback: Option<Fallback>,
    /// State of the caller's own row: `none`/`declared`/`active`.
    pub status: CredentialStatus,
    /// State of the effective row this reference resolves to.
    pub inheritance: InheritanceStatus,
    /// Monotonic version of the caller's own row; `None` iff `status: none`.
    pub version: Option<i64>,
    /// Last-write instant of the caller's own row; `None` iff `status: none`.
    pub updated_at: Option<OffsetDateTime>,
    /// Subject id that created the caller's own row; `None` iff `status:
    /// none` (the effective record is inherited from an ancestor with no
    /// own row in play). Populated by the *own* row alone — even when the
    /// effective value is inherited (`inheritance: inherited` with a
    /// `declared` own row) — never by an ancestor's row: an inherited
    /// entry must not disclose identifiers from another tenant, and a
    /// caller who needs the ancestor's owner acts in that tenant's context
    /// and reads it there.
    pub owner_id: Option<OwnerId>,
    /// Expiry instant of the effective record, when the type is expirable
    /// and one was set.
    pub expires_at: Option<OffsetDateTime>,
    /// The caller's own row's validator (the strong `ETag` source);
    /// `None` iff the caller's tenant holds no row under the reference, in
    /// which case the REST layer serves a weak, opaque `ETag` instead.
    pub validator: Option<Validator>,
}

/// The value, with exactly what is needed to use it (ADR-0004, "Two
/// representations"): nothing administrative rides along — `sharing`,
/// `inheritance`, `status` stay on [`Credential`]. `GET
/// /credentials/{ref}/secret`'s response shape.
#[derive(Debug)]
#[allow(
    clippy::struct_field_names,
    reason = "secret_type names the field precisely; Secret is the resource this schema is for"
)]
pub struct Secret {
    /// The reference this value was resolved through.
    pub reference: SecretRef,
    /// The resolved value's full GTS type id (a consumer must know whether
    /// it is parsing a `basic_auth` object or an `api_key` string).
    pub secret_type: String,
    /// Expiry instant, when the type is expirable and one was set.
    pub expires_at: Option<OffsetDateTime>,
    /// The decrypted value.
    pub value: SecretValue,
    /// The winning row's validator — travels with the value so a caller that
    /// reads and rotates its own credential never needs the record address.
    pub validator: Validator,
}

/// Body of [`CredStoreClientV1::put`](crate::CredStoreClientV1::put) — a
/// whole-credential replace: fields absent reset to their defaults (ADR-0004,
/// "Two write verbs"). `value` is always required; a `PUT` never produces a
/// `declared` record.
#[derive(Debug)]
pub struct CredentialWrite {
    /// Full GTS type id. Required on create (defaults are not inferred here —
    /// the REST layer defaults to the generic type before constructing this);
    /// on replace it must equal the stored type (`TYPE_IMMUTABLE` otherwise).
    pub secret_type: Option<GtsId>,
    /// Sharing mode for the written record.
    pub sharing: SharingMode,
    /// Suppression policy; defaults to [`Fallback::Inherit`].
    pub fallback: Fallback,
    /// Expiry instant. `None` clears any stored expiry — a `PUT` is a whole
    /// replace, so an absent `expires_at` on the wire means "no expiry",
    /// exactly as an omitted field on today's shipped `PUT` already clears
    /// one.
    pub expires_at: Option<OffsetDateTime>,
    /// The value to write. Always required — a `PUT` without one is a 400
    /// (`VALUE_REQUIRED`) at the REST boundary.
    pub value: SecretValue,
}

/// Tri-state field for an RFC 7396 JSON Merge Patch: absent (untouched),
/// explicit `null` (remove/clear), or a value (replace). Pair with
/// `#[serde(default, deserialize_with = "…")]` at the REST DTO boundary — see
/// `credstore::api::rest::dto::deserialize_double_option`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PatchField<T> {
    /// The key was not present in the merge-patch body; leave untouched.
    #[default]
    Absent,
    /// The key was present with JSON `null`; remove/clear the field.
    Null,
    /// The key was present with a value; replace the field with it.
    Set(T),
}

impl<T> PatchField<T> {
    /// `true` iff the key was not present in the body at all.
    #[must_use]
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

/// Body of [`CredStoreClientV1::patch`](crate::CredStoreClientV1::patch) — an
/// RFC 7396 JSON Merge Patch over the mutable fields of [`Credential`] plus
/// `value` (ADR-0004). A field absent from every one of these is untouched; a
/// `PatchField::Null` on `expires_at`/`value` clears/removes it. `sharing`,
/// `fallback` and `secret_type` have no `Null` state on the wire — a
/// merge-patch `null` for any of them is a REST-layer 400
/// (`NULL_NOT_ALLOWED`), since none is a nullable column.
#[derive(Debug, Default)]
pub struct CredentialPatch {
    /// Present only to be compared against the stored type; a differing
    /// value is `TYPE_IMMUTABLE`. Never actually changes the stored type.
    pub secret_type: Option<GtsId>,
    /// New sharing mode, if the merge-patch body named one.
    pub sharing: Option<SharingMode>,
    /// New suppression policy, if the merge-patch body named one.
    pub fallback: Option<Fallback>,
    /// Expiry change: absent (untouched), `Null` (clear), or `Set` (replace).
    pub expires_at: PatchField<OffsetDateTime>,
    /// Value change: absent (untouched), `Null` (remove — the record becomes
    /// `declared`), or `Set` (rotate/create the value).
    pub value: PatchField<SecretValue>,
}

impl CredentialPatch {
    /// `true` when the patch touches nothing at all — every field is
    /// [`PatchField::Absent`] and no metadata key was named. Rejected by the
    /// domain with `EMPTY_PATCH` (400): `PATCH {}` is meaningless, not a
    /// no-op success.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.secret_type.is_none()
            && self.sharing.is_none()
            && self.fallback.is_none()
            && self.expires_at.is_absent()
            && self.value.is_absent()
    }
}

/// One item of the collection read (`CredStoreClientV1::list`, ADR-0005): the
/// reduced [`Credential`] a reference resolves to, plus its decrypted value
/// (`secret`) when the request ran in **value mode**
/// (`$select` containing `secret`, ADR-0004 "Bulk secret read: the collection
/// in value mode"). `secret` is `None` in ordinary (metadata-mode) listing —
/// the collection never carries a value unless the caller opted into value
/// mode, and even then only for the items whose value the caller may read.
#[derive(Debug)]
pub struct CredentialListItem {
    /// The reduced credential record (ADR-0005, "Reducing a reference to one
    /// item"): one item per reference, matching what a point read
    /// (`CredStoreClientV1::get`) of that reference would resolve to.
    pub credential: Credential,
    /// The decrypted value, present only in value mode and only for an item
    /// the caller may read (`read_secret`); a refused, missing, or
    /// fingerprint-mismatched item is omitted from the page entirely rather
    /// than carrying `None` here.
    pub secret: Option<SecretValue>,
}

/// Outcome of one [`CredStoreMaintenanceV1::run_gc`](crate::CredStoreMaintenanceV1::run_gc)
/// invocation (ADR-0006). Mirrors the gear-internal report the domain service
/// returns; the gear's `ClientHub` adapter converts between the two.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Expired `active` rows removed (maintenance job pass 1).
    pub expired_deleted: u64,
    /// Versions deleted by the gc drain (`reason != pending`: superseded,
    /// removed, or aborted).
    pub gc_deleted: u64,
    /// Orphaned `pending` versions reclaimed (older than
    /// `gc.pending_max_age_secs` and unreferenced by any row).
    pub gc_pending_reclaimed: u64,
}

#[cfg(test)]
mod models_tests {
    use super::*;

    #[test]
    fn secret_ref_accepts_valid_shapes() {
        for ok in ["partner-openai-key", "api_key_v2", "ABC123", "ok-key_1"] {
            assert!(SecretRef::new(ok).is_ok(), "{ok} should be valid");
        }
    }

    #[test]
    fn secret_ref_rejects_invalid_chars_and_empty() {
        assert!(SecretRef::new("").is_err());
        for bad in ["has:colon", "my key", "key/path"] {
            assert!(SecretRef::new(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn secret_ref_length_boundary() {
        // 255 is the inclusive max; 256 is rejected (boundary both sides).
        assert!(SecretRef::new("a".repeat(255)).is_ok());
        assert!(SecretRef::new("a".repeat(256)).is_err());
    }

    #[test]
    fn secret_ref_deserialize_validates() {
        // The custom Deserialize is the real wire path (a raw JSON string must
        // go through the same validation as `SecretRef::new`).
        let valid: Result<SecretRef, _> = serde_json::from_str("\"valid-key_1\"");
        assert_eq!(valid.expect("valid").as_ref(), "valid-key_1");
        assert!(serde_json::from_str::<SecretRef>("\"my:evil/key\"").is_err());
        assert!(serde_json::from_str::<SecretRef>("\"\"").is_err());
        assert!(serde_json::from_str::<SecretRef>(&format!("\"{}\"", "a".repeat(256))).is_err());
    }

    #[test]
    fn secret_ref_serde_round_trips() {
        let r = SecretRef::new("round-trip").expect("valid");
        let json = serde_json::to_string(&r).expect("serialize");
        assert_eq!(json, "\"round-trip\"");
        let back: SecretRef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.as_ref(), "round-trip");
    }

    #[test]
    fn secret_value_redacts() {
        let v = SecretValue::from("supersecret");
        assert_eq!(format!("{v:?}"), "[REDACTED]");
        assert_eq!(format!("{v}"), "[REDACTED]");
        assert_eq!(v.as_bytes(), b"supersecret");
    }

    #[test]
    fn value_id_new_v4_is_random_and_displays_as_uuid() {
        let a = ValueId::new_v4();
        let b = ValueId::new_v4();
        assert_ne!(a, b, "each mint must be store-wide unique");
        assert_eq!(a.0.get_version_num(), 4);
        assert_eq!(a.to_string(), a.0.to_string());
    }

    #[test]
    fn sharing_mode_default_is_tenant() {
        assert_eq!(SharingMode::default(), SharingMode::Tenant);
    }

    #[test]
    fn sharing_mode_serde_round_trips() {
        for (mode, expected) in [
            (SharingMode::Private, "\"private\""),
            (SharingMode::Tenant, "\"tenant\""),
            (SharingMode::Shared, "\"shared\""),
        ] {
            let json = serde_json::to_string(&mode).expect("serialize");
            assert_eq!(json, expected);
            let back: SharingMode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn fallback_default_is_inherit_and_wire_form_is_lowercase() {
        assert_eq!(Fallback::default(), Fallback::Inherit);
        for (f, expected) in [
            (Fallback::Inherit, "\"inherit\""),
            (Fallback::None, "\"none\""),
        ] {
            let json = serde_json::to_string(&f).expect("serialize");
            assert_eq!(json, expected);
            let back: Fallback = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, f);
        }
    }

    #[test]
    fn credential_status_wire_form_is_lowercase() {
        for (s, expected) in [
            (CredentialStatus::None, "\"none\""),
            (CredentialStatus::Declared, "\"declared\""),
            (CredentialStatus::Active, "\"active\""),
        ] {
            let json = serde_json::to_string(&s).expect("serialize");
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn inheritance_status_wire_form_is_lowercase() {
        for (s, expected) in [
            (InheritanceStatus::Own, "\"own\""),
            (InheritanceStatus::Inherited, "\"inherited\""),
            (InheritanceStatus::Overridden, "\"overridden\""),
            (InheritanceStatus::Suppressed, "\"suppressed\""),
        ] {
            let json = serde_json::to_string(&s).expect("serialize");
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn patch_field_default_is_absent() {
        assert!(PatchField::<i64>::default().is_absent());
        assert!(!PatchField::<i64>::Null.is_absent());
        assert!(!PatchField::Set(3_i64).is_absent());
    }

    #[test]
    fn credential_patch_is_empty_iff_every_field_is_untouched() {
        assert!(CredentialPatch::default().is_empty());

        assert!(
            !CredentialPatch {
                sharing: Some(SharingMode::Shared),
                ..CredentialPatch::default()
            }
            .is_empty()
        );

        assert!(
            !CredentialPatch {
                expires_at: PatchField::Null,
                ..CredentialPatch::default()
            }
            .is_empty()
        );

        assert!(
            !CredentialPatch {
                value: PatchField::Set(SecretValue::from("x")),
                ..CredentialPatch::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn gc_report_default_is_all_zeros() {
        let report = GcReport::default();
        assert_eq!(report.expired_deleted, 0);
        assert_eq!(report.gc_deleted, 0);
        assert_eq!(report.gc_pending_reclaimed, 0);
    }

    #[test]
    fn credential_list_item_carries_no_secret_in_metadata_mode() {
        let credential = Credential {
            reference: SecretRef::new("ref").expect("valid"),
            secret_type: "gts.cf.core.credstore.credential.v1~cf.core.credstore.generic.v1~"
                .to_owned(),
            sharing: SharingMode::Tenant,
            fallback: Some(Fallback::Inherit),
            status: CredentialStatus::Active,
            inheritance: InheritanceStatus::Own,
            version: Some(1),
            updated_at: None,
            owner_id: Some(OwnerId::nil()),
            expires_at: None,
            validator: None,
        };
        let item = CredentialListItem {
            credential,
            secret: None,
        };
        assert!(item.secret.is_none());
    }
}
