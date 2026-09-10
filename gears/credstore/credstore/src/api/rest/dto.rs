//! REST DTOs for the credstore module (ADR-0004: the credential surface).

use credstore_sdk::{
    Credential, CredentialStatus, Fallback, InheritanceStatus, Secret, SharingMode,
};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Sharing mode for the REST transport layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(response, request)]
pub enum SharingModeDto {
    /// Only the owner can access the credential.
    Private,
    /// Any actor inside the owning tenant can access the credential.
    #[default]
    Tenant,
    /// Descendant tenants can inherit the credential.
    Shared,
}

impl From<SharingMode> for SharingModeDto {
    fn from(value: SharingMode) -> Self {
        match value {
            SharingMode::Private => Self::Private,
            SharingMode::Tenant => Self::Tenant,
            SharingMode::Shared => Self::Shared,
        }
    }
}

impl From<SharingModeDto> for SharingMode {
    fn from(value: SharingModeDto) -> Self {
        match value {
            SharingModeDto::Private => Self::Private,
            SharingModeDto::Tenant => Self::Tenant,
            SharingModeDto::Shared => Self::Shared,
        }
    }
}

/// Suppression policy for the REST transport layer (ADR-0004, "Suppression").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(response, request)]
pub enum FallbackDto {
    /// Resolution keeps walking up the tenant chain past this row.
    #[default]
    Inherit,
    /// This row blocks resolution outright when it is the nearest candidate.
    None,
}

impl From<Fallback> for FallbackDto {
    fn from(value: Fallback) -> Self {
        match value {
            Fallback::Inherit => Self::Inherit,
            Fallback::None => Self::None,
        }
    }
}

impl From<FallbackDto> for Fallback {
    fn from(value: FallbackDto) -> Self {
        match value {
            FallbackDto::Inherit => Self::Inherit,
            FallbackDto::None => Self::None,
        }
    }
}

/// The caller's own-row status for the REST transport layer (ADR-0004, "Two
/// representations").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum CredentialStatusDto {
    /// The caller's tenant holds no row under the reference at all.
    None,
    /// The caller's own row exists but carries no value.
    Declared,
    /// The caller's own row exists and carries a value.
    Active,
}

impl From<CredentialStatus> for CredentialStatusDto {
    fn from(value: CredentialStatus) -> Self {
        match value {
            CredentialStatus::None => Self::None,
            CredentialStatus::Declared => Self::Declared,
            CredentialStatus::Active => Self::Active,
        }
    }
}

/// The effective-row inheritance status for the REST transport layer
/// (ADR-0004, "Two representations").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum InheritanceStatusDto {
    Own,
    Inherited,
    Overridden,
    Suppressed,
}

impl From<InheritanceStatus> for InheritanceStatusDto {
    fn from(value: InheritanceStatus) -> Self {
        match value {
            InheritanceStatus::Own => Self::Own,
            InheritanceStatus::Inherited => Self::Inherited,
            InheritanceStatus::Overridden => Self::Overridden,
            InheritanceStatus::Suppressed => Self::Suppressed,
        }
    }
}

/// Serde helper for RFC 7396 JSON-Merge-Patch tri-state fields: distinguishes
/// an omitted key (`None`), an explicit JSON `null` (`Some(None)`), and a
/// value (`Some(Some(v))`). Pair with
/// `#[serde(default, deserialize_with = "deserialize_double_option")]`.
#[allow(clippy::option_option)]
pub(crate) fn deserialize_double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}

/// Request body for `PUT /credstore/v1/credentials/{ref}` (ADR-0004): a
/// whole-credential replace. `value` is modelled as optional at the wire
/// shape (`Option<String>`) so its *absence* can be distinguished from a
/// malformed body and rejected with the typed `VALUE_REQUIRED` reason rather
/// than a generic deserialization error.
///
/// `Debug` is hand-written to redact `value`.
#[derive(Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PutCredentialRequestDto {
    /// Full GTS type id. Required on create; on replace it must equal the
    /// stored type (`TYPE_IMMUTABLE` otherwise).
    #[serde(default, rename = "type")]
    pub secret_type: Option<String>,
    /// Sharing mode — required (ADR-0004).
    pub sharing: SharingModeDto,
    /// Suppression policy; defaults to `inherit`.
    #[serde(default)]
    pub fallback: FallbackDto,
    /// Expiry instant (RFC 3339); only for expirable types. A `PUT` is a
    /// whole-value replace: omitting `expires_at` clears a stored expiry.
    #[serde(default)]
    #[schema(format = DateTime)]
    pub expires_at: Option<String>,
    /// The value to write. Required — its absence is `400 VALUE_REQUIRED`.
    #[serde(default)]
    pub value: Option<String>,
}

impl std::fmt::Debug for PutCredentialRequestDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PutCredentialRequestDto")
            .field("secret_type", &self.secret_type)
            .field("sharing", &self.sharing)
            .field("fallback", &self.fallback)
            .field("expires_at", &self.expires_at)
            .field("value", &self.value.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// Request body for `PATCH /credstore/v1/credentials/{ref}` (ADR-0004): an
/// RFC 7396 JSON Merge Patch. Every field is tri-state (`Option<Option<T>>`):
/// absent (untouched), explicit `null`, or a value. `secret_type`/`sharing`/
/// `fallback` have no valid `null` state on the wire (rejected as
/// `NULL_NOT_ALLOWED` by the handler, since none is a nullable column);
/// `expires_at`/`value` do — `null` clears/removes them.
///
/// `Debug` is hand-written to redact `value`.
#[derive(Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
#[allow(clippy::option_option)]
pub struct CredentialPatchDto {
    #[serde(
        default,
        rename = "type",
        deserialize_with = "deserialize_double_option"
    )]
    #[schema(value_type = Option<String>)]
    pub secret_type: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    #[schema(value_type = Option<SharingModeDto>)]
    pub sharing: Option<Option<SharingModeDto>>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    #[schema(value_type = Option<FallbackDto>)]
    pub fallback: Option<Option<FallbackDto>>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    #[schema(value_type = Option<String>)]
    pub expires_at: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    #[schema(value_type = Option<String>)]
    pub value: Option<Option<String>>,
}

impl std::fmt::Debug for CredentialPatchDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialPatchDto")
            .field("secret_type", &self.secret_type)
            .field("sharing", &self.sharing)
            .field("fallback", &self.fallback)
            .field("expires_at", &self.expires_at)
            .field(
                "value",
                &match &self.value {
                    None => "<absent>",
                    Some(None) => "<null>",
                    Some(Some(_)) => "[REDACTED]",
                },
            )
            .finish()
    }
}

/// Response body for `GET /credstore/v1/credentials/{ref}` (ADR-0004): the
/// credential record. Never carries the value — see [`SecretDto`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct CredentialDto {
    pub reference: String,
    #[serde(rename = "type")]
    pub secret_type: String,
    pub sharing: SharingModeDto,
    /// The caller's own row's suppression policy; absent when the caller
    /// holds no row under the reference (`status: none`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<FallbackDto>,
    pub status: CredentialStatusDto,
    pub inheritance: InheritanceStatusDto,
    /// The caller's own row's version; absent iff `status` is `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
    /// The caller's own row's last-write instant; absent iff `status` is
    /// `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(format = DateTime)]
    pub updated_at: Option<String>,
    /// Subject id that created the caller's own row; absent iff `status` is
    /// `none`. Never populated from an ancestor's row — an inherited entry
    /// must not disclose identifiers from another tenant, and a caller who
    /// needs the ancestor's owner acts in that tenant's context and reads
    /// it there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(format = DateTime)]
    pub expires_at: Option<String>,
}

impl CredentialDto {
    /// Convert the domain [`Credential`] into the REST DTO shape.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Internal`] if `updated_at`/`expires_at` fail to
    /// format as RFC 3339 (never expected in practice).
    pub fn try_from_credential(c: &Credential) -> Result<Self, DomainError> {
        let updated_at = c
            .updated_at
            .map(|at| {
                at.format(&time::format_description::well_known::Rfc3339)
                    .map_err(|e| DomainError::internal(format!("updated_at failed to format: {e}")))
            })
            .transpose()?;
        let expires_at = c
            .expires_at
            .map(|at| {
                at.format(&time::format_description::well_known::Rfc3339)
                    .map_err(|e| DomainError::internal(format!("expires_at failed to format: {e}")))
            })
            .transpose()?;
        Ok(Self {
            reference: c.reference.as_ref().to_owned(),
            secret_type: c.secret_type.clone(),
            sharing: c.sharing.into(),
            fallback: c.fallback.map(Into::into),
            status: c.status.into(),
            inheritance: c.inheritance.into(),
            version: c.version,
            updated_at,
            owner_id: c.owner_id.map(|o| o.0.to_string()),
            expires_at,
        })
    }
}

/// Response body for `GET /credstore/v1/credentials/{ref}/secret`
/// (ADR-0004): the value with exactly what is needed to use it.
///
/// `Debug` is hand-written to redact `value`.
#[derive(Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct SecretDto {
    pub reference: String,
    #[serde(rename = "type")]
    pub secret_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(format = DateTime)]
    pub expires_at: Option<String>,
    pub value: String,
}

impl std::fmt::Debug for SecretDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretDto")
            .field("reference", &self.reference)
            .field("type", &self.secret_type)
            .field("expires_at", &self.expires_at)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

impl SecretDto {
    /// Convert the domain [`Secret`] into the REST DTO shape.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Internal`] when the value is not valid UTF-8
    /// (e.g. binary written via the SDK). The JSON/string transport cannot
    /// represent it, and lossy decoding would silently corrupt the secret, so
    /// we reject rather than mangle.
    pub fn try_from_secret(s: &Secret) -> Result<Self, DomainError> {
        let value = String::from_utf8(s.value.as_bytes().to_vec()).map_err(|_| {
            DomainError::internal(
                "secret value is not valid UTF-8 and cannot be encoded for the REST transport",
            )
        })?;
        let expires_at = s
            .expires_at
            .map(|at| {
                at.format(&time::format_description::well_known::Rfc3339)
                    .map_err(|e| DomainError::internal(format!("expires_at failed to format: {e}")))
            })
            .transpose()?;
        Ok(Self {
            reference: s.reference.as_ref().to_owned(),
            secret_type: s.secret_type.clone(),
            expires_at,
            value,
        })
    }
}

/// The weak, opaque `ETag` for a `Credential` whose caller holds no own row
/// (ADR-0004, D4): `W/"<16-hex of sha256(tenant_id|reference|winner
/// id|winner version)>"`. Changes exactly when the winning (ancestor's) row
/// changes; never accepted as a valid `If-Match` (RFC 9110 requires the
/// strong comparison there).
#[must_use]
pub fn weak_etag(tenant_id: Uuid, reference: &str, winner_id: Uuid, winner_version: i64) -> String {
    let input = format!("{tenant_id}|{reference}|{winner_id}|{winner_version}");
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, input.as_bytes());
    let hex: String = digest.as_ref()[..8]
        .iter()
        .fold(String::new(), |mut acc, b| {
            use std::fmt::Write as _;
            // Writing into a String cannot fail.
            let _written = write!(acc, "{b:02x}");
            acc
        });
    format!("W/\"{hex}\"")
}

#[cfg(test)]
#[path = "dto_tests.rs"]
mod tests;
