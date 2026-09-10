//! `IdP` user-operations DTOs.
//!
//! Sibling of [`crate::idp`] (tenant-side) — hosts the request /
//! response / failure shapes consumed by the user-lifecycle half of
//! [`crate::idp::IdpPluginClient`]. The trait itself lives in
//! `crate::idp` so a single `Arc<dyn IdpPluginClient>` carries
//! both tenant and user methods.
//!
//! Users are NOT modelled in AM storage per
//! `cpt-cf-account-management-constraint-no-user-storage` -- every
//! user-side call is a live pass-through to the resolved provider
//! plugin.
//!
//! # `IdpUser` projection schema
//!
//! [`IdpUser`] mirrors the published GTS schema
//! `gts.cf.core.am.user.v1~` declared in
//! `gears/system/account-management/docs/schemas/user.v1.schema.json`.
//! The shape is intentionally tenant-minimal: no credentials, no
//! `IdP`-internal identifiers, no membership cache. Provider plugins
//! project only profile-like fields the `IdP` exposes.
//!
//! # Failure model
//!
//! [`IdpUserOperationFailure`] discriminates between the categories AM's
//! service layer maps onto the public error envelope:
//!
//! * [`IdpUserOperationFailure::Unavailable`] -- transport failure or
//!   timeout; AM surfaces `idp_unavailable` per
//!   `cpt-cf-account-management-dod-idp-user-operations-contract-idp-unavailability-contract`.
//!   AM holds NO fallback projection, so `list_users` during an outage
//!   returns the envelope-mapped error rather than a stale page.
//! * [`IdpUserOperationFailure::UnsupportedOperation`] -- provider
//!   declines a mutating operation (read-only / legacy provider). AM
//!   surfaces `idp_unsupported_operation`. Providers MUST NOT silently
//!   no-op a mutating call; surface the variant explicitly.
//! * [`IdpUserOperationFailure::Rejected`] -- provider returned a
//!   payload-rejection category (e.g. duplicate username, malformed
//!   email). AM surfaces a generic validation envelope; the canonical
//!   error catalog is owned by `feature-errors-observability` (sibling
//!   feature) and may refine the mapping in a follow-up.
//!
//! `provider_detail` strings carried by the failure variants are
//! routed through AM's redaction pipeline before reaching public
//! envelopes (see `cf-account-management::domain::idp` for the
//! `into_domain_error` boundary). Plugin authors do not need to redact
//! themselves -- they pass the raw vendor text and AM owns the public-
//! surface mapping.

use gts::GtsTypeId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use toolkit_odata_macros::ODataFilterable;
use uuid::Uuid;

/// Resolved tenant snapshot surfaced to provider plugins so they can
/// route to `IdP`-specific identifiers (Keycloak realm, Zitadel
/// organization, vendor-side org id, etc.).
///
/// AM resolves the tenant via `TenantService` and loads any opaque
/// plugin-private state from `tenant_idp_metadata` before invoking
/// the contract; the resolved snapshot is forwarded here so the
/// plugin does not round-trip back to AM for data it could receive
/// inline.
///
/// # Field set
///
/// * `tenant_id` — the stable identifier the plugin keys vendor-side
///   state on.
/// * `tenant_name` — the current human-readable label. AM treats it
///   as mutable (tenant rename is allowed) so the value reflects
///   AM's source of truth at call time, not a snapshot from the
///   provisioning call.
/// * `tenant_type` — the resolved chained GTS identifier
///   (e.g. `gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~`).
///   Mandatory: AM treats a Types Registry outage as a service-level
///   failure on user-ops (surfaces as `ServiceUnavailable`) rather
///   than leaking an `Option` into the plugin contract. Immutable
///   post-creation.
/// * `metadata` — opaque plugin-private blob persisted by AM in
///   `tenant_idp_metadata` and replayed on every subsequent `IdP` call
///   for this tenant. Returned by the plugin from
///   [`crate::idp::IdpProvisionResult::metadata`]; `None` when the
///   plugin owns no per-tenant state. AM does NOT interpret the
///   shape; the plugin is the sole owner.
///
/// # Why these fields and not more
///
/// The four fields are the minimum AM commits to surface uniformly
/// on every "operates on existing tenant" call (`provision_user`,
/// `deprovision_user`, `list_users`, `deprovision_tenant`). Anything
/// else the plugin needs it stashes inside `metadata` at
/// `provision_tenant` time and reads back from there.
///
/// # Adding fields
///
/// `#[non_exhaustive]` lets the SDK add fields (`parent_id`,
/// resolver-specific descriptors, etc.) in a minor release without
/// breaking compilation. New fields default to safe values on
/// constructors that predate them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
#[allow(
    clippy::struct_field_names,
    reason = "every field IS tenant-scoped (id / name / type / metadata) and stripping the prefix loses the public contract that the value comes from AM-resolved tenant state"
)]
pub struct IdpTenantContext {
    /// Stable tenant identifier (the same UUID stored in AM's
    /// `tenants.id` column).
    pub tenant_id: Uuid,
    /// Human-readable tenant name, useful for plugins that derive
    /// provider-side identifiers from the tenant label (e.g. a
    /// Keycloak realm whose name follows the tenant slug).
    pub tenant_name: String,
    /// Resolved tenant type as a chained `GtsTypeId`
    /// (e.g. `gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~`).
    /// Mandatory at the contract boundary — AM treats failures of
    /// the underlying Types Registry reverse-resolve as service-
    /// level errors rather than leaking an `Option` into the plugin.
    pub tenant_type: GtsTypeId,
    /// Opaque plugin-private metadata persisted by AM in
    /// `tenant_idp_metadata`. AM replays whatever the plugin returned
    /// from [`crate::idp::IdpProvisionResult::metadata`] on every
    /// subsequent `IdP` call for this tenant. `None` when the plugin
    /// returned no metadata, or before the plugin has been called
    /// for this tenant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl IdpTenantContext {
    /// Build a [`IdpTenantContext`] from the resolved tenant snapshot.
    #[must_use]
    pub fn new(
        tenant_id: Uuid,
        tenant_name: impl Into<String>,
        tenant_type: GtsTypeId,
        metadata: Option<Value>,
    ) -> Self {
        Self {
            tenant_id,
            tenant_name: tenant_name.into(),
            tenant_type,
            metadata,
        }
    }
}

/// Profile-minimal payload accepted by
/// [`IdpPluginClient::provision_user`].
///
/// Shape mirrors the published `gts.cf.core.am.user.v1~` projection
/// minus the `IdP`-issued `id` (the provider assigns it on success).
/// The structural contract (field shapes, `minLength` / `maxLength`,
/// `format`) is owned by the JSON Schema referenced above; the AM
/// service layer validates instances against that schema at runtime
/// via the GTS Types Registry — see
/// `cf-account-management::domain::gts_validation::validate_new_user_payload_via_gts`.
#[derive(Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IdpNewUser {
    /// Login identifier (REQUIRED per the published schema).
    pub username: String,
    /// Optional contact email surfaced through the projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Optional display name surfaced through the projection.
    /// Plugin authors derive this from vendor-specific shapes —
    /// Keycloak combines `firstName`/`lastName`; OIDC providers
    /// fold `given_name`/`family_name`/`name` claims. When
    /// `first_name`/`last_name` are also supplied the plugin should
    /// prefer the granular fields and ignore `display_name` (or
    /// derive it as `"{first_name} {last_name}"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Optional given name. Forwarded verbatim to the `IdP` (Keycloak
    /// `firstName`). Some `IdP`s require it for "account fully set up"
    /// validation before password grants will succeed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,
    /// Optional family name. Forwarded verbatim to the `IdP` (Keycloak
    /// `lastName`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    /// Optional initial password supplied atomically with user creation.
    /// When `Some`, the `IdP` plugin sets the credential during user
    /// creation so the caller can immediately password-grant on the new
    /// account. When `None`, the plugin creates the user without a
    /// password — the operator is expected to drive password setup via
    /// an out-of-band flow (admin reset, reset-email, etc.). The value
    /// is redacted in `Debug`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<NewUserPassword>,
}

/// Initial credential bundle for [`IdpNewUser::password`].
///
/// `value` is redacted in `Debug` so accidental log emission of a
/// `IdpProvisionUserRequest`-shaped payload never leaks the secret.
/// Plugins that build a vendor-specific payload should consume `value`
/// directly via field access — do not derive `Debug`-stringified forms
/// for logging.
#[derive(Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct NewUserPassword {
    /// Plaintext password as the operator supplied it. Stays plaintext
    /// only for the duration of the create call — the plugin forwards
    /// it to the `IdP` and never persists it on the AM side.
    pub value: String,
    /// `true` → the credential is marked temporary at the `IdP`. For
    /// Keycloak this attaches `UPDATE_PASSWORD` to the user's required
    /// actions so the next interactive sign-in forces a rotation.
    /// `false` → permanent credential, password grants succeed without
    /// an intermediate `UPDATE_PASSWORD` step.
    #[serde(default)]
    pub temporary: bool,
}

impl std::fmt::Debug for NewUserPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewUserPassword")
            .field("value", &"<redacted>")
            .field("temporary", &self.temporary)
            .finish()
    }
}

impl std::fmt::Debug for IdpNewUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdpNewUser")
            .field("username", &self.username)
            .field("email", &self.email)
            .field("display_name", &self.display_name)
            .field("first_name", &self.first_name)
            .field("last_name", &self.last_name)
            .field("password", &self.password)
            .finish()
    }
}

impl IdpNewUser {
    /// Construct a payload with only the required `username`. Use
    /// the `with_*` setters to populate optional profile fields.
    #[must_use]
    pub fn new(username: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            email: None,
            display_name: None,
            first_name: None,
            last_name: None,
            password: None,
        }
    }

    #[must_use]
    pub fn with_email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }

    #[must_use]
    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }

    #[must_use]
    pub fn with_first_name(mut self, first_name: impl Into<String>) -> Self {
        self.first_name = Some(first_name.into());
        self
    }

    #[must_use]
    pub fn with_last_name(mut self, last_name: impl Into<String>) -> Self {
        self.last_name = Some(last_name.into());
        self
    }

    /// Attach an initial password. Pass `temporary=true` to force
    /// `UPDATE_PASSWORD` on the user's first interactive sign-in.
    #[must_use]
    pub fn with_password(mut self, value: impl Into<String>, temporary: bool) -> Self {
        self.password = Some(NewUserPassword {
            value: value.into(),
            temporary,
        });
        self
    }
}

/// Public projection of the `gts.cf.core.am.user.v1~` GTS schema.
///
/// The shape is the contract for downstream consumers (e.g.
/// `feature-user-groups` membership existence checks, audit pipeline).
/// No `IdP`-internal identifier, credential, or membership cache is
/// included per `cpt-cf-account-management-adr-idp-user-identity-source-of-truth`
/// and `cpt-cf-account-management-adr-idp-user-tenant-binding`.
///
/// The GTS schema identifier is exported as
/// [`crate::gts::USER_RESOURCE_TYPE`] for use in canonical-error
/// envelopes and PEP rules. The struct itself is not annotated with
/// `gts_macros::struct_to_gts_schema` because the macro's "base
/// type" contract requires the struct to carry either a
/// `GtsInstanceId`-typed `id` field or a `GtsTypeId`-typed
/// `gts_type` field — `IdpUser.id` is the `IdP`-issued domain UUID
/// (Keycloak's user UUID, etc.), not a GTS instance identifier, so
/// the two semantics are intentionally distinct.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IdpUser {
    /// `IdP`-issued UUID user identifier. The provider owns the issuance
    /// and the value is stable across the user's lifetime in this
    /// tenant scope.
    pub id: Uuid,
    /// Login identifier (REQUIRED per the schema).
    pub username: String,
    /// Optional contact email.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Optional display name. Plugin authors derive this from
    /// vendor-specific shapes (Keycloak `firstName`+`lastName`,
    /// OIDC `name`/`given_name`/`family_name` claims, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Optional given name (Keycloak `firstName`, OIDC `given_name`).
    /// Plugin projects it from the vendor profile when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,
    /// Optional family name (Keycloak `lastName`, OIDC `family_name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
}

impl IdpUser {
    /// Construct a projection with the two required schema fields.
    /// Use the `with_*` setters for optional profile fields.
    #[must_use]
    pub fn new(id: Uuid, username: impl Into<String>) -> Self {
        Self {
            id,
            username: username.into(),
            email: None,
            display_name: None,
            first_name: None,
            last_name: None,
        }
    }

    #[must_use]
    pub fn with_email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }

    #[must_use]
    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }

    #[must_use]
    pub fn with_first_name(mut self, first_name: impl Into<String>) -> Self {
        self.first_name = Some(first_name.into());
        self
    }

    #[must_use]
    pub fn with_last_name(mut self, last_name: impl Into<String>) -> Self {
        self.last_name = Some(last_name.into());
        self
    }
}

/// Pagination parameters for [`IdpPluginClient::list_users`].
///
/// Cursor-based, wire-compatible with `toolkit_odata::Page<T>` (same
/// envelope `cf-gears-resource-group-sdk` and the AM REST surface
/// use). At the SPI layer the cursor is an **opaque token owned by
/// the `IdP` plugin** — AM never inspects it on the plugin-call
/// side.
///
/// At the **AM REST boundary**, however, the current `OData`
/// extractor parses the wire `cursor=` query param via
/// [`toolkit_odata::CursorV1::decode`]. That narrows the contract:
/// today's REST-facing cursors MUST be `CursorV1`-shaped. The
/// `static-idp-plugin` plugin honours this end-to-end (encodes a
/// key-tuple `CursorV1` on every page boundary, validates drift via
/// `validate_cursor_against`). A future plugin wrapping a vendor SDK
/// (e.g. Zitadel `next_token`, Keycloak plugin-encoded
/// `(filter_hash, offset)`) would need to either:
///   * encode its native token into a `CursorV1.k` slot (the simplest
///     path; the AM REST layer stays unchanged), OR
///   * expose a parallel REST surface that bypasses the `OData`
///     extractor for cursor handling.
///
/// A plugin backed by a SQL store SHOULD embed a filter hash and a
/// stable sort key in the cursor (see [`toolkit_odata::pagination`])
/// so that a client switching `$filter` mid-pagination receives a
/// deterministic invalid-cursor error instead of silently jumping
/// pages.
///
/// `top` and `cursor` are private; construction goes through
/// [`IdpUserPagination::new`] / [`IdpUserPagination::default`]. Read via
/// [`IdpUserPagination::top`] and [`IdpUserPagination::cursor`].
///
/// `top = 0` would turn a tenant-scoped existence check
/// (`$filter=id eq <uuid>` --
/// `cpt-cf-account-management-flow-idp-user-operations-contract-list-users`)
/// into a false-negative empty page on providers that honor the
/// literal value, since AM cannot disambiguate "user absent" from
/// "page size was zero".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "RawUserPagination")]
#[non_exhaustive]
pub struct IdpUserPagination {
    top: u32,
    cursor: Option<String>,
}

impl IdpUserPagination {
    /// Default page size used by [`IdpUserPagination::default`]. Chosen
    /// to match the AM tenant-CRUD listing default and stay below
    /// [`Self::MAX_TOP`].
    pub const DEFAULT_TOP: u32 = 50;

    /// Upper bound enforced by [`IdpUserPagination::new`]. Matches the
    /// `OpenAPI Top.maximum` typical value referenced in the AM tenant-
    /// CRUD listing surface. Mirrors the DB-side row-count ceiling so a
    /// misbehaving (or compromised) `IdP` plugin asked to return
    /// `top = u32::MAX` users cannot exhaust the page-buffer allocation.
    pub const MAX_TOP: u32 = 200;

    /// Cap on the opaque cursor string AM is willing to forward. The
    /// cursor is plugin-shaped — AM never parses it — but caps the
    /// length so a hostile / buggy plugin cannot recycle a request
    /// into an unbounded heap allocation through the AM proxy. `4
    /// KiB` is generous for `toolkit_odata`-shaped cursors
    /// (`{filter_hash, last_seen_id, limit}` base64) and for
    /// vendor-native tokens (Zitadel `next_token` is bytes; Keycloak
    /// plugin-encoded `(filter_hash, offset)` fits easily).
    pub const MAX_CURSOR_LEN: usize = 4096;

    /// Serde-attribute helper: returns [`Self::DEFAULT_TOP`]. Used by
    /// `RawUserPagination` so a wire payload that omits `top` still
    /// produces a non-zero page size when routed through
    /// [`IdpUserPagination::new`]. Without this helper, omitting `top`
    /// would fail deserialization before `TryFrom` could substitute
    /// the default, contradicting the documented "default top = 50"
    /// contract.
    #[must_use]
    const fn default_top() -> u32 {
        Self::DEFAULT_TOP
    }

    /// Construct a validated pagination.
    ///
    /// # Errors
    ///
    /// * [`IdpUserPaginationError::TopMustBePositive`] when `top` is zero.
    /// * [`IdpUserPaginationError::TopExceedsMax`] when `top` exceeds
    ///   [`Self::MAX_TOP`] — guards against a caller forwarding
    ///   `top = u32::MAX` straight to the `IdP` plugin.
    /// * [`IdpUserPaginationError::CursorTooLong`] when the supplied
    ///   cursor exceeds [`Self::MAX_CURSOR_LEN`] bytes.
    pub fn new(top: u32, cursor: Option<String>) -> Result<Self, IdpUserPaginationError> {
        if top == 0 {
            return Err(IdpUserPaginationError::TopMustBePositive);
        }
        if top > Self::MAX_TOP {
            return Err(IdpUserPaginationError::TopExceedsMax {
                requested: top,
                max: Self::MAX_TOP,
            });
        }
        if let Some(ref c) = cursor
            && c.len() > Self::MAX_CURSOR_LEN
        {
            return Err(IdpUserPaginationError::CursorTooLong {
                len: c.len(),
                max: Self::MAX_CURSOR_LEN,
            });
        }
        Ok(Self { top, cursor })
    }

    /// Pagination shape for the authoritative single-user existence check
    /// (`$filter=id eq <uuid>`): `top = 1`, no cursor. Bypasses the
    /// validation in [`Self::new`] because the literal `top = 1` is
    /// trivially valid; this lets the helper stay `const`.
    #[must_use]
    pub const fn for_existence_check() -> Self {
        Self {
            top: 1,
            cursor: None,
        }
    }

    /// Read-only access to the validated `top`. Always `>= 1` per the
    /// constructor invariant.
    #[must_use]
    pub const fn top(&self) -> u32 {
        self.top
    }

    /// Read-only access to the opaque, plugin-shaped cursor (if any).
    /// The first page of a listing has `cursor = None`; subsequent
    /// pages echo back the `next_cursor` the plugin returned in the
    /// previous [`toolkit_odata::Page::page_info`].
    #[must_use]
    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }
}

impl Default for IdpUserPagination {
    fn default() -> Self {
        Self {
            top: Self::DEFAULT_TOP,
            cursor: None,
        }
    }
}

/// Wire shape for [`IdpUserPagination`] deserialization. Mirrors the
/// public fields but skips the `top > 0` invariant -- the
/// [`TryFrom<RawUserPagination>`] impl below routes the value
/// through [`IdpUserPagination::new`] so the invariant is enforced on
/// every serde input path, not just constructor calls.
///
/// `top` defaults to [`IdpUserPagination::DEFAULT_TOP`] when absent in the
/// wire payload (matching the [`Default`] impl on `IdpUserPagination`);
/// `cursor` defaults to `None`. Without the `top` default, a wire payload
/// like `{"cursor": "..."}` would fail deserialization before the `TryFrom`
/// could substitute the configured default, contradicting the
/// documented "default top = 50" contract.
#[derive(Debug, Clone, Deserialize)]
struct RawUserPagination {
    #[serde(default = "IdpUserPagination::default_top")]
    top: u32,
    #[serde(default)]
    cursor: Option<String>,
}

impl TryFrom<RawUserPagination> for IdpUserPagination {
    type Error = IdpUserPaginationError;

    fn try_from(raw: RawUserPagination) -> Result<Self, Self::Error> {
        Self::new(raw.top, raw.cursor)
    }
}

/// Validation errors reported by [`IdpUserPagination::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdpUserPaginationError {
    /// `top` was zero; the user-list contract treats `top` as a
    /// strict positive page size so an existence-check filter cannot
    /// silently return an empty page.
    TopMustBePositive,
    /// `top` exceeded [`IdpUserPagination::MAX_TOP`]. Caps the page size
    /// AM is willing to ask an `IdP` plugin for, so a literal
    /// `top = u32::MAX` cannot trigger an unbounded allocation in the
    /// provider.
    TopExceedsMax { requested: u32, max: u32 },
    /// The opaque cursor exceeded [`IdpUserPagination::MAX_CURSOR_LEN`].
    /// AM forwards cursors as-is, so capping the length is the only
    /// defense against a hostile / buggy plugin trying to recycle a
    /// listing into an unbounded heap allocation through the AM
    /// proxy.
    CursorTooLong { len: usize, max: usize },
}

impl core::fmt::Display for IdpUserPaginationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TopMustBePositive => f.write_str("top must be at least 1"),
            Self::TopExceedsMax { requested, max } => {
                write!(f, "top {requested} exceeds maximum {max}")
            }
            Self::CursorTooLong { len, max } => {
                write!(f, "cursor length {len} exceeds maximum {max}")
            }
        }
    }
}

impl core::error::Error for IdpUserPaginationError {}

/// Request shape for [`IdpPluginClient::provision_user`].
///
/// `tenant_context.tenant_id` is the tenant the user is being
/// provisioned into; AM has already validated the scope is `Active`
/// before invoking the contract. There is intentionally no separate
/// `tenant_id` field on the request — carrying both a top-level
/// `tenant_id` and `tenant_context.tenant_id` would make it ambiguous
/// which is authoritative; the context is the single source of truth.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IdpProvisionUserRequest {
    /// Resolved tenant context (id, name, optional chained type).
    pub tenant_context: IdpTenantContext,
    /// Profile-minimal payload to forward into the `IdP`.
    pub payload: IdpNewUser,
}

impl IdpProvisionUserRequest {
    #[must_use]
    pub const fn new(tenant_context: IdpTenantContext, payload: IdpNewUser) -> Self {
        Self {
            tenant_context,
            payload,
        }
    }
}

/// Request shape for [`IdpPluginClient::deprovision_user`].
///
/// `tenant_context.tenant_id` is the tenant scope; see
/// [`IdpProvisionUserRequest`] for the duplication-removal rationale.
/// The resolved tenant context is forwarded on every contract method
/// per `cpt-cf-account-management-algo-idp-user-operations-contract-idp-contract-invocation`
/// step `package-request`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IdpDeprovisionUserRequest {
    pub tenant_context: IdpTenantContext,
    pub user_id: Uuid,
}

impl IdpDeprovisionUserRequest {
    #[must_use]
    pub const fn new(tenant_context: IdpTenantContext, user_id: Uuid) -> Self {
        Self {
            tenant_context,
            user_id,
        }
    }
}

/// Partial-update payload for [`crate::idp::IdpPluginClient::update_user`].
///
/// JSON Merge Patch (RFC 7396) tri-state, lowered from the REST DTO by
/// the AM service layer:
///
/// * `username`: `None` leaves the login identifier unchanged; `Some(v)`
///   renames it. The login identifier is REQUIRED per the published
///   `gts.cf.core.am.user.v1~` schema, so it can never be cleared — the
///   REST boundary rejects an explicit `null`.
/// * nullable profile fields (`email` / `display_name` / `first_name` /
///   `last_name`): `None` = leave unchanged, `Some(None)` = clear the
///   field, `Some(Some(v))` = set it.
/// * `password`: `None` = unchanged; `Some(pw)` sets a new credential.
///   Not part of the user projection; the `IdP` enforces its own
///   password policy. Redacted in `Debug` via [`NewUserPassword`].
///
/// The AM service rejects an all-`None` patch as a validation error
/// before the plugin is invoked (see [`Self::is_empty`]).
#[derive(Debug, Clone, Default)]
#[allow(clippy::option_option)]
#[non_exhaustive]
pub struct IdpUserPatch {
    /// Rename the login identifier. `None` = unchanged.
    pub username: Option<String>,
    /// `Some(None)` clears the email, `Some(Some(v))` sets it.
    pub email: Option<Option<String>>,
    /// `Some(None)` clears the display name, `Some(Some(v))` sets it.
    pub display_name: Option<Option<String>>,
    /// `Some(None)` clears the given name, `Some(Some(v))` sets it.
    pub first_name: Option<Option<String>>,
    /// `Some(None)` clears the family name, `Some(Some(v))` sets it.
    pub last_name: Option<Option<String>>,
    /// `Some(pw)` sets a new credential; `None` leaves it unchanged.
    pub password: Option<NewUserPassword>,
}

impl IdpUserPatch {
    /// An empty patch (every field `None`). Populate via field
    /// assignment or the `with_*` builders.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when the patch carries no field. The AM service maps this
    /// to a validation error rather than issuing a no-op `IdP` call.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.username.is_none()
            && self.email.is_none()
            && self.display_name.is_none()
            && self.first_name.is_none()
            && self.last_name.is_none()
            && self.password.is_none()
    }

    /// Builder: rename the login identifier.
    #[must_use]
    pub fn with_username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Builder: set (`Some`) or clear (`None`) the email.
    #[must_use]
    pub fn with_email(mut self, email: Option<String>) -> Self {
        self.email = Some(email);
        self
    }

    /// Builder: set (`Some`) or clear (`None`) the display name.
    #[must_use]
    pub fn with_display_name(mut self, display_name: Option<String>) -> Self {
        self.display_name = Some(display_name);
        self
    }

    /// Builder: set (`Some`) or clear (`None`) the given name.
    #[must_use]
    pub fn with_first_name(mut self, first_name: Option<String>) -> Self {
        self.first_name = Some(first_name);
        self
    }

    /// Builder: set (`Some`) or clear (`None`) the family name.
    #[must_use]
    pub fn with_last_name(mut self, last_name: Option<String>) -> Self {
        self.last_name = Some(last_name);
        self
    }

    /// Builder: attach a new password credential. Pass `temporary=true`
    /// to force `UPDATE_PASSWORD` on the user's next interactive sign-in.
    #[must_use]
    pub fn with_password(mut self, value: impl Into<String>, temporary: bool) -> Self {
        self.password = Some(NewUserPassword {
            value: value.into(),
            temporary,
        });
        self
    }
}

/// Request shape for [`crate::idp::IdpPluginClient::update_user`].
///
/// `tenant_context.tenant_id` is the tenant scope; see
/// [`IdpProvisionUserRequest`] for the duplication-removal rationale.
/// The resolved tenant context is forwarded on every contract method
/// per `cpt-cf-account-management-algo-idp-user-operations-contract-idp-contract-invocation`
/// step `package-request`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IdpUpdateUserRequest {
    /// Resolved tenant context (id, name, optional chained type).
    pub tenant_context: IdpTenantContext,
    /// `IdP`-issued identifier of the user being patched.
    pub user_id: Uuid,
    /// JSON Merge Patch over the user's mutable attributes.
    pub patch: IdpUserPatch,
}

impl IdpUpdateUserRequest {
    #[must_use]
    pub const fn new(tenant_context: IdpTenantContext, user_id: Uuid, patch: IdpUserPatch) -> Self {
        Self {
            tenant_context,
            user_id,
            patch,
        }
    }
}

/// Request shape for [`crate::idp::IdpPluginClient::list_users`].
///
/// `filter` and `order` carry the validated `OData` translation handed
/// down from the AM service layer. Plugins MUST honour both; the
/// existence-check / point-lookup contract previously expressed as
/// `user_id_filter` is now `$filter=id eq <uuid>` with `top = 1`.
/// Caller MUST NOT change `filter` or `order` between continuation
/// requests with the same opaque cursor — plugins do not detect drift.
///
/// Both `Page(len = 1)` and `Page(len = 0)` are success outcomes per
/// `cpt-cf-account-management-flow-idp-user-operations-contract-list-users`;
/// plugins MUST NOT surface "user absent" as an error. An empty page
/// is the canonical "absent" signal for the `id eq <uuid>` filter shape.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IdpListUsersRequest {
    pub tenant_context: IdpTenantContext,
    pub pagination: IdpUserPagination,
    pub filter: Option<toolkit_odata::filter::FilterNode<IdpUserFilterField>>,
    pub order: Option<toolkit_odata::ODataOrderBy>,
}

impl IdpListUsersRequest {
    /// Construct a request with no filter / order.
    #[must_use]
    pub const fn new(tenant_context: IdpTenantContext, pagination: IdpUserPagination) -> Self {
        Self {
            tenant_context,
            pagination,
            filter: None,
            order: None,
        }
    }

    /// Builder: attach a typed filter.
    #[must_use]
    pub fn with_filter(
        mut self,
        filter: toolkit_odata::filter::FilterNode<IdpUserFilterField>,
    ) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Builder: attach an order.
    #[must_use]
    pub fn with_order(mut self, order: toolkit_odata::ODataOrderBy) -> Self {
        self.order = Some(order);
        self
    }
}

/// Failure discriminant shared by every user-operations contract
/// method.
///
/// AM's service layer maps each variant onto the canonical error
/// taxonomy via `cf-account-management::domain::idp` (the redaction +
/// public-envelope boundary). Plugin authors do not need to redact
/// `detail` themselves -- AM owns the public-surface mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdpUserOperationFailure {
    /// Provider was unreachable, the call timed out, or the transport
    /// returned a retryable failure. AM maps this to the public
    /// `idp_unavailable` code; no fallback projection is served per
    /// `cpt-cf-account-management-constraint-no-user-storage`.
    Unavailable { detail: String },
    /// Provider declined the operation in its current implementation
    /// profile (typically a read-only or legacy provider that does
    /// not support mutating user operations). AM maps this to
    /// `idp_unsupported_operation`. Providers MUST NOT silently no-op
    /// a mutating call.
    UnsupportedOperation { detail: String },
    /// Provider returned a payload-rejection the plugin could NOT
    /// classify further. AM maps this to the canonical validation
    /// envelope with the generic `request`/`VALIDATION` tokens.
    /// Providers SHOULD emit the classified variants below
    /// ([`Self::DuplicateUser`], [`Self::PasswordPolicy`]) whenever
    /// the vendor response identifies the cause — this catch-all is
    /// the fallback for genuinely unattributable rejections.
    Rejected { detail: String },
    /// Provider rejected the operation because a uniqueness invariant
    /// on `field` is already taken (duplicate username in
    /// the realm, duplicate email realm-wide). AM maps this to the
    /// canonical `already_exists` envelope (HTTP 409) — the create
    /// contract already declares a Conflict response and documents
    /// idempotency keyed by `(tenant_id, username)`.
    DuplicateUser {
        field: IdpUserDuplicateField,
        detail: String,
    },
    /// Provider rejected the supplied password against its configured
    /// password policy (length / complexity / history). AM maps this
    /// to the canonical validation envelope with the structured
    /// `password` / `PASSWORD_POLICY` field-violation tokens so
    /// clients can attribute the failure to the password field
    ///; the raw policy text stays provider-side.
    PasswordPolicy { detail: String },
    /// Provider reported the target user does not exist in this tenant
    /// scope. Unlike `deprovision_user` — which folds a vendor-side
    /// "user does not exist" response into `Ok(())` for idempotency —
    /// `update_user` MUST surface absence: a PATCH against a missing
    /// user is a `404`, not a silent no-op. AM maps this to the
    /// canonical `not_found` envelope.
    NotFound { detail: String },
    /// Provider refused to write every attribute in `fields` because
    /// they are IdP-managed and not overridable through AM -- e.g. the
    /// realm federates them from a read-only LDAP mapper, or the
    /// provider's user-profile configuration marks them non-writable.
    ///
    /// Distinct from [`Self::UnsupportedOperation`] (the provider does
    /// not implement `update_user` *at all*, HTTP 501) and from
    /// [`Self::Rejected`] (the value was bad, not the field). This is a
    /// per-field capability fact, identical for every caller, so it is
    /// NOT a permission decision -- AM maps it to the canonical
    /// validation envelope carrying one structured
    /// `<field>` / `IDP_MANAGED_FIELD` field-violation per entry (HTTP
    /// 400) so a client can attribute the refusal to the exact inputs
    /// and disable them.
    ///
    /// `fields` is a *set*, not a single attribute: a merge patch can
    /// touch five attributes at once and a read-only federated realm
    /// typically locks several of them, so a provider MUST report every
    /// refused attribute the patch touched in one failure rather than
    /// making the client discover them one round-trip at a time.
    /// Providers MUST NOT emit an empty `fields` -- a refusal with
    /// nothing to attribute it to is an unclassified rejection and
    /// belongs in [`Self::Rejected`]; AM degrades an empty set to the
    /// generic validation envelope.
    FieldNotWritable {
        /// The refused attributes. Non-empty; order is not significant
        /// and duplicates are ignored by AM's boundary.
        fields: Vec<IdpUserAttribute>,
        detail: String,
    },
}

/// A writable user profile attribute, used to attribute an
/// [`IdpUserOperationFailure::FieldNotWritable`] refusal to the exact
/// request field. `#[non_exhaustive]`: the writable surface grows with
/// [`crate::IdpUserPatch`] without a breaking SDK release.
///
/// `password` is deliberately absent: a provider refusing a credential
/// write reports it through [`IdpUserOperationFailure::PasswordPolicy`],
/// which already owns the `password` / `PASSWORD_POLICY` tokens.
///
/// `Hash` is part of the contract: providers hold their non-writable
/// attributes as a set, so a `HashSet<IdpUserAttribute>` is the natural
/// shape on the implementing side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IdpUserAttribute {
    /// The login identifier (`username`).
    Username,
    /// The email address (`email`).
    Email,
    /// The display name (`display_name`).
    DisplayName,
    /// The given name (`first_name`).
    FirstName,
    /// The family name (`last_name`).
    LastName,
}

impl IdpUserAttribute {
    /// Stable wire token for canonical `field_violations[].field`.
    /// MUST match the corresponding `UserUpdateRequest` JSON property
    /// name -- clients key form-field attribution off this string.
    #[must_use]
    pub const fn as_field_token(self) -> &'static str {
        match self {
            Self::Username => "username",
            Self::Email => "email",
            Self::DisplayName => "display_name",
            Self::FirstName => "first_name",
            Self::LastName => "last_name",
        }
    }

    /// Human phrase for curated public error details ("the {phrase} is
    /// managed by the identity provider..."). Centralised here so AM's
    /// canonical boundary and its tests cannot drift.
    #[must_use]
    pub const fn as_human_phrase(self) -> &'static str {
        match self {
            Self::Username => "username",
            Self::Email => "email address",
            Self::DisplayName => "display name",
            Self::FirstName => "first name",
            Self::LastName => "last name",
        }
    }
}

/// Which unique user attribute an [`IdpUserOperationFailure::DuplicateUser`]
/// rejection collided on. `#[non_exhaustive]`: providers may learn to
/// classify further unique attributes without a breaking SDK release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdpUserDuplicateField {
    /// Realm-scoped username collision.
    Username,
    /// Realm-wide email collision.
    Email,
    /// The provider reported a uniqueness conflict without an
    /// attributable single field. Keycloak's `ModelDuplicateException`
    /// path emits the combined constant "User exists with same
    /// username or email" — on KC 26 that is the ONLY 409 shape
    /// `createUser` produces directly — and a conflict body the plugin
    /// cannot parse lands here too: on the user-create endpoint a 409
    /// has no other cause than a uniqueness collision, so the status
    /// alone justifies the conflict classification even when the field
    /// stays unknown.
    UsernameOrEmail,
}

impl IdpUserDuplicateField {
    /// Stable wire/field token for canonical `field_violations.field`
    /// / `resource_name` and metric labels.
    #[must_use]
    pub const fn as_field_token(self) -> &'static str {
        match self {
            Self::Username => "username",
            Self::Email => "email",
            Self::UsernameOrEmail => "username_or_email",
        }
    }

    /// Human phrase for curated public error details ("a user with
    /// this {phrase} already exists"). Centralised here so AM's
    /// canonical boundary and its tests cannot drift.
    #[must_use]
    pub const fn as_human_phrase(self) -> &'static str {
        match self {
            Self::Username => "username",
            Self::Email => "email",
            Self::UsernameOrEmail => "username or email",
        }
    }
}

impl IdpUserOperationFailure {
    /// Stable, snake-case metric-label form of this variant. Used by
    /// AM-side observability when emitting per-call outcome metrics
    /// (the metric catalog itself is owned by
    /// `feature-errors-observability`); kept on the SDK type so
    /// producers do not duplicate the variant -> string mapping in
    /// match arms.
    #[must_use]
    pub const fn as_metric_label(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => "unavailable",
            Self::UnsupportedOperation { .. } => "unsupported_operation",
            Self::Rejected { .. } => "rejected",
            Self::DuplicateUser { .. } => "duplicate_user",
            Self::PasswordPolicy { .. } => "password_policy",
            Self::NotFound { .. } => "not_found",
            Self::FieldNotWritable { .. } => "field_not_writable",
        }
    }

    /// Raw provider-supplied `detail` string carried by every variant.
    /// Mirrors [`crate::idp::IdpProvisionFailure::detail`] and
    /// [`crate::idp::IdpDeprovisionFailure::detail`] so consumers read
    /// the detail uniformly across all `IdP` failure enums.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Unavailable { detail }
            | Self::UnsupportedOperation { detail }
            | Self::Rejected { detail }
            | Self::DuplicateUser { detail, .. }
            | Self::PasswordPolicy { detail }
            | Self::NotFound { detail }
            | Self::FieldNotWritable { detail, .. } => detail,
        }
    }
}

impl core::fmt::Display for IdpUserOperationFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.as_metric_label(), self.detail())
    }
}

impl core::error::Error for IdpUserOperationFailure {}

/// AM-client request to [`crate::idp::IdpPluginClient::list_users`]
/// via the AM-side `UserService`. Caller-controlled filter and order;
/// the AM service injects a default order + `id ASC` tiebreaker before
/// forwarding to the plugin.
///
/// Both `Page(len = 1)` and `Page(len = 0)` are success outcomes per
/// `cpt-cf-account-management-flow-idp-user-operations-contract-list-users`;
/// empty page is the canonical "absent" signal for the
/// `$filter=id eq <uuid>` point-lookup shape.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ListUsersQuery {
    pub pagination: IdpUserPagination,
    pub filter: Option<toolkit_odata::filter::FilterNode<IdpUserFilterField>>,
    pub order: Option<toolkit_odata::ODataOrderBy>,
}

impl ListUsersQuery {
    /// Build one bounded ID-set lookup through the authorized `list_users` API.
    /// IDs are sorted and deduplicated; providers must apply the exact UUID set,
    /// not return an unfiltered page. Follow any returned cursor with the same
    /// filter before interpreting missing IDs as absent. Unsupported providers
    /// must return an explicit error, not ignore the filter.
    ///
    /// # Errors
    /// Empty sets or more than `IdpUserPagination::MAX_TOP` distinct IDs violate
    /// the pagination bounds. Consumers should chunk larger sets.
    pub fn with_ids(ids: impl IntoIterator<Item = Uuid>) -> Result<Self, IdpUserPaginationError> {
        let ids: std::collections::BTreeSet<_> = ids.into_iter().collect();
        let top = u32::try_from(ids.len()).unwrap_or(u32::MAX);
        let pagination = IdpUserPagination::new(top, None)?;
        Ok(
            Self::new(pagination).with_filter(toolkit_odata::filter::FilterNode::InList {
                field: IdpUserFilterField::Id,
                values: ids
                    .into_iter()
                    .map(toolkit_odata::filter::ODataValue::Uuid)
                    .collect(),
            }),
        )
    }

    /// Construct a query with the given pagination and no filter / order.
    #[must_use]
    pub const fn new(pagination: IdpUserPagination) -> Self {
        Self {
            pagination,
            filter: None,
            order: None,
        }
    }

    /// Builder: attach a typed filter.
    #[must_use]
    pub fn with_filter(
        mut self,
        filter: toolkit_odata::filter::FilterNode<IdpUserFilterField>,
    ) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Builder: attach an order.
    #[must_use]
    pub fn with_order(mut self, order: toolkit_odata::ODataOrderBy) -> Self {
        self.order = Some(order);
        self
    }

    /// Ergonomic helper for the authoritative single-user
    /// existence-check shape. Pre-builds
    /// `$filter = id eq <uuid>` and pins pagination to `top = 1`,
    /// `cursor = None`. The AM service-layer applies the defensive
    /// returned-id guard against contract drift from a plugin that
    /// silently ignores the filter.
    #[must_use]
    pub fn with_id(id: Uuid) -> Self {
        let filter = toolkit_odata::filter::FilterNode::binary(
            IdpUserFilterField::Id,
            toolkit_odata::filter::FilterOp::Eq,
            toolkit_odata::filter::ODataValue::Uuid(id),
        );
        Self {
            pagination: IdpUserPagination::for_existence_check(),
            filter: Some(filter),
            order: None,
        }
    }
}

/// Filter-fields definition struct that feeds the
/// [`toolkit_odata_macros::ODataFilterable`] derive. Exists only to
/// generate [`IdpUserQueryFilterField`] (re-exported as
/// [`IdpUserFilterField`]) — the user-facing request shape is
/// [`ListUsersQuery`], not this struct. Mirrors the
/// [`crate::tenant::TenantInfoQuery`] convention.
#[derive(ODataFilterable)]
#[allow(dead_code)]
pub struct IdpUserQuery {
    /// User UUID. Filter with `$filter=id eq <uuid>` for the
    /// authoritative point-lookup / existence-check shape.
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    /// Login identifier (Keycloak `username`, OIDC `preferred_username`).
    #[odata(filter(kind = "String"))]
    pub username: String,
    /// Email address (Keycloak `email`, OIDC `email`).
    #[odata(filter(kind = "String"))]
    pub email: String,
    /// Display name surfaced through the projection. Plugins typically
    /// derive it (Keycloak: concat `firstName`/`lastName`; OIDC: `name`
    /// claim).
    #[odata(filter(kind = "String"))]
    pub display_name: String,
    /// Given name (Keycloak `firstName`, OIDC `given_name`).
    #[odata(filter(kind = "String"))]
    pub first_name: String,
    /// Family name (Keycloak `lastName`, OIDC `family_name`).
    #[odata(filter(kind = "String"))]
    pub last_name: String,
}

pub use IdpUserQueryFilterField as IdpUserFilterField;

#[cfg(test)]
#[path = "idp_user_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "idp_user_query_tests.rs"]
mod query_tests;
