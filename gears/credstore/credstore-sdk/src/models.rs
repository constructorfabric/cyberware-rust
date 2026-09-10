// Updated: 2026-04-07 by Constructor Tech
// Updated: 2026-03-18 by Constructor Tech
//! Public credential references, values, metadata, sharing, expiry, and write
//! preconditions.
//!
//! Secret values redact formatting output and zeroize their bytes on drop.
use std::fmt;

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
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

/// How a write should treat the secret's expiry.
///
/// Modelled as an explicit tri-state rather than `Option` so that "leave the
/// stored expiry untouched" and "remove the stored expiry" are distinct: with a
/// plain `Option<OffsetDateTime>`, `None` conflates the two and an ordinary
/// value rotation (`put`) would silently strip an existing expiry. Expiry is
/// only permitted for types whose traits are `expirable`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExpiryWrite {
    /// Keep the currently stored expiry on overwrite; no expiry on create.
    /// This is the default, so the convenience
    /// [`CredStoreClientV1::put`](crate::CredStoreClientV1::put) never clears an
    /// expiry as a side effect of a value rotation.
    #[default]
    Preserve,
    /// Set the expiry to this instant (must be in the future at write time).
    Set(time::OffsetDateTime),
    /// Remove any stored expiry.
    Clear,
}

impl ExpiryWrite {
    /// The instant this write introduces as a *new* expiry, if any. Only
    /// [`Self::Set`] introduces one; [`Self::Preserve`]/[`Self::Clear`] do not,
    /// so they never trigger expiry validation against the secret's type.
    #[must_use]
    pub fn requested(self) -> Option<time::OffsetDateTime> {
        match self {
            Self::Set(at) => Some(at),
            Self::Preserve | Self::Clear => None,
        }
    }

    /// Resolve the value to persist against the currently stored expiry
    /// (`current` — `None` on create). `Preserve` keeps `current`, `Set`
    /// overrides it, `Clear` drops it.
    #[must_use]
    pub fn resolve(self, current: Option<time::OffsetDateTime>) -> Option<time::OffsetDateTime> {
        match self {
            Self::Preserve => current,
            Self::Set(at) => Some(at),
            Self::Clear => None,
        }
    }
}

/// Optimistic-concurrency precondition for a write or delete — the in-process
/// equivalent of the REST `If-Match` header, and a **required** argument of
/// every update/delete: there are no unconditional overwrites. Read the
/// current generation from a prior [`GetSecretResponse`] (`id` + `version`)
/// and send [`Self::Matches`]; a failed precondition surfaces as
/// [`CredStoreError::Conflict`].
///
/// [`Self::Exists`] is the deliberate, visible-in-code opt-out for blind
/// create-or-replace flows that cannot hold a version (rotation /
/// provisioning, where the new value is not derived from the stored one).
/// It is also what a caller recovering a fence-poisoned reference (ADR-0003)
/// uses, since a fenced `GET` fails closed with 404 and yields no validator
/// to hold — but under immutable value versions (ADR-0006) that recovery is
/// an ordinary new write like any other, not a special healing path;
/// `Exists` carries no meaning beyond RFC 9110 last-writer-wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePrecondition {
    /// The target secret must already exist (REST `If-Match: *`). This is an
    /// explicit last-writer-wins overwrite: concurrent `Exists` writers to one
    /// reference race and the later write survives wholesale — each lands
    /// under its own immutable value version (ADR-0006), so the race
    /// resolves into two intact versions and one pointer, never a corrupted
    /// value. Reserve it for writers that own their references outright
    /// (rotation, provisioning) and for a caller with no version to hold (a
    /// fenced `GET` fails closed with no validator to read); read-modify-write
    /// callers must use [`Self::Matches`].
    Exists,
    /// Compare-and-set: the current generation must still be `(id, version)`
    /// (REST `If-Match: "<id>.<version>"`). `id` is the row UUID — fresh per
    /// recreated secret — so a validator from an earlier generation never
    /// matches after a delete/recreate even if the version counters coincide.
    Matches {
        /// Row (generation) UUID from the observed [`GetSecretResponse::id`].
        id: uuid::Uuid,
        /// Version counter from the observed [`GetSecretResponse::version`].
        version: i64,
    },
}

/// Options for typed writes ([`CredStoreClientV1::put_opts`](crate::CredStoreClientV1::put_opts) /
/// [`create_opts`](crate::CredStoreClientV1::create_opts)).
#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    /// Full GTS type id of the secret type (e.g. via
    /// [`SecretType::gts_id`](crate::SecretType::gts_id) for a built-in, or a
    /// custom type's id). `None` keeps the existing secret's type on overwrite
    /// and defaults to the generic type on create. The gear resolves it to the
    /// type's deterministic UUID and validates existence against the
    /// types-registry.
    pub secret_type: Option<GtsId>,
    /// Expiry write intent — see [`ExpiryWrite`]. Defaults to
    /// [`ExpiryWrite::Preserve`], so a write that does not mention expiry
    /// leaves any stored expiry unchanged.
    pub expires_at: ExpiryWrite,
}

/// Response returned by [`CredStoreClientV1::get`](crate::CredStoreClientV1::get)
/// containing the secret value and access metadata.
#[derive(Debug)]
pub struct GetSecretResponse {
    /// The decrypted secret value.
    pub value: SecretValue,
    /// Generation id of the resolved secret — the metadata row's UUID, minted
    /// fresh for every recreated secret. Combined with `version` it forms the
    /// gear's strong `ETag` (`"<id>.<version>"`), so a validator from a
    /// deleted-and-recreated secret's earlier generation can never match the
    /// current one even when the version counters coincide.
    pub id: uuid::Uuid,
    /// The tenant that owns this secret (may differ from the requesting tenant
    /// when the secret is inherited via hierarchical resolution).
    pub owner_tenant_id: TenantId,
    /// The sharing mode of the secret.
    pub sharing: SharingMode,
    /// `true` if the secret was retrieved from an ancestor tenant via
    /// hierarchical resolution, `false` if owned by the requesting tenant.
    pub is_inherited: bool,
    /// Monotonic version of the resolved secret within its generation;
    /// surfaced as the HTTP `ETag` together with `id`.
    pub version: i64,
    /// The secret's full GTS type id, as resolved from the types-registry
    /// (covers dynamically registered custom types; use
    /// [`crate::types::SecretType::from_gts_id`] to recover a catalog type
    /// when needed).
    pub secret_type: String,
    /// Expiry instant, when the type is expirable and one was set.
    pub expires_at: Option<time::OffsetDateTime>,
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
}
