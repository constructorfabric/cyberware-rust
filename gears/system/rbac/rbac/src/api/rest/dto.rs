//! REST DTOs for the RBAC module's HTTP surface.
//!
//! These types mirror the wire shape of the `rbac_sdk::models::*` records.
//! They exist as a separate set of types — rather than reusing the SDK
//! records — for two reasons:
//!
//! * `rbac-sdk` is an infrastructure-free contract; pulling
//!   `utoipa::ToSchema` into the SDK would force every consumer
//!   (including non-HTTP ones) to compile the `OpenAPI` machinery.
//! * The SDK enum `PrincipalType` carries deliberate `PascalCase` wire
//!   tags. `#[toolkit_macros::api_dto]` applies `rename_all = "snake_case"`
//!   unconditionally, which would change those tags. The enum mirror uses
//!   plain `Serialize`/`Deserialize`/`ToSchema` derives to preserve them.
//!
//! Most conversions are infallible; the SDK `Scope` is rendered through
//! its canonical `path()` shape (e.g. `/tenants/{uuid}`). The lone
//! exception is `PrincipalType` → `PrincipalTypeDto` (and the
//! `RoleAssignment` → `RoleAssignmentDto` wrapper that calls it), which
//! is `TryFrom`: `PrincipalType` is `#[non_exhaustive]`, so a future
//! unmapped variant surfaces as `RbacServiceError::Internal` (rendered
//! 500 server-side) rather than being silently coerced to `User`.

// Every DTO carries `#[toolkit_macros::api_dto(...)]` which requires `pub`
// for its OpenAPI registration. The crate is module-internal, so the
// `unreachable_pub` lint fires under elevated sweeps — silence it here
// rather than fight the macro contract.
#![allow(unreachable_pub)]

// Every `#[api_dto(request)]` struct MUST carry
// `#[serde(deny_unknown_fields)]` so a typo in a request body
// (e.g. `permisions` for `permissions`) fails fast with a 400
// `invalid_argument` rather than being silently dropped on the
// way to the service layer — combined with `Option<Option<String>>`
// "unchanged vs clear" semantics, a silent drop produces a
// "200 OK, nothing changed" response that is painful to debug.
// `toolkit_macros::api_dto` does NOT inject the attribute on its
// own; every request DTO in this file declares it explicitly.
// When adding a new request DTO, add the attribute too.

use chrono::{DateTime, Utc};
use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::{PermissionRule, PrincipalType, RoleAssignment, RoleDefinition, Scope};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::permission_catalog::AuthzPermission;
use crate::domain::role_definition_repo::RoleTypeCounts;

// ---------------------------------------------------------------------------
// Enums (manual derives to preserve PascalCase wire tags from the SDK)
// ---------------------------------------------------------------------------

/// Wire form of [`PrincipalType`]. `PascalCase` tags `"User"` / `"Group"` /
/// `"ServicePrincipal"` are preserved from the SDK.
// DE0203 is intentionally bypassed here — `api_dto` would force
// `rename_all = "snake_case"`, breaking the documented PascalCase wire tags.
#[allow(unknown_lints, de0203_dtos_must_use_api_dto)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum PrincipalTypeDto {
    User,
    Group,
    ServicePrincipal,
}

impl TryFrom<PrincipalType> for PrincipalTypeDto {
    type Error = RbacServiceError;

    fn try_from(p: PrincipalType) -> Result<Self, Self::Error> {
        match p {
            PrincipalType::User => Ok(PrincipalTypeDto::User),
            PrincipalType::Group => Ok(PrincipalTypeDto::Group),
            PrincipalType::ServicePrincipal => Ok(PrincipalTypeDto::ServicePrincipal),
            // `PrincipalType` is `#[non_exhaustive]` per SDK convention,
            // so this wildcard is mandatory. Fail instead of silently
            // coercing to `User`: an unmapped variant means a dependency
            // upgrade landed a new SDK variant without an accompanying
            // DTO update, and the response shape would otherwise
            // misrepresent authorisation data on the wire (e.g. rendering
            // a future `Device` principal as a `User`). Surfacing it as a
            // 500 `Internal` (fallible lowering) keeps the wire response
            // honest without crashing the process the way `unreachable!`
            // would on a live request.
            other => Err(RbacServiceError::Internal {
                message: format!(
                    "unmapped PrincipalType variant {other:?} reached DTO conversion; \
                     SDK widened without a `PrincipalTypeDto` update"
                ),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Role-definition DTOs
// ---------------------------------------------------------------------------

/// REST mirror of [`PermissionRule`]. The Allow / Deny class is encoded
/// by which array (`permissions` vs `not_permissions`) it lives in on
/// the surrounding entity — the DTO itself only carries the
/// `(operation, target_type)` pair.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct PermissionRuleDto {
    pub operation: String,
    pub target_type: String,
}

impl From<PermissionRule> for PermissionRuleDto {
    fn from(rule: PermissionRule) -> Self {
        Self {
            operation: rule.operation,
            target_type: rule.target_type,
        }
    }
}

impl From<PermissionRuleDto> for PermissionRule {
    fn from(dto: PermissionRuleDto) -> Self {
        PermissionRule::new(dto.operation, dto.target_type)
    }
}

/// REST mirror of [`RoleDefinition`]. UUID and timestamp fields surface
/// as strings in the `OpenAPI` document.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RoleDefinitionDto {
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub is_built_in: bool,
    pub permissions: Vec<PermissionRuleDto>,
    pub not_permissions: Vec<PermissionRuleDto>,
    /// Canonical scope path strings. Kept `Vec<String>` rather than the SDK's
    /// typed `Vec<Scope>` so the generated `OpenAPI` schema — and therefore the
    /// published HTTP contract — is unaffected by the domain-side typing.
    pub assignable_scopes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    pub owner_tenant_id: Option<Uuid>,
    #[schema(value_type = String, format = "date-time")]
    pub created_at: DateTime<Utc>,
    #[schema(value_type = String, format = "date-time")]
    pub updated_at: DateTime<Utc>,
    pub created_by: String,
    /// How many role assignments reference this role, counted over **only
    /// the assignments the caller may read** — never platform-wide, so a
    /// tenant admin cannot learn how many grants of a built-in role exist
    /// elsewhere on the platform.
    ///
    /// The key is **omitted** rather than sent as `0` when no count exists
    /// for this caller: they have no assignment-read visibility anywhere, or
    /// the response came from a write path (`POST` / `PATCH`), which performs
    /// no count. A present `0` is a real answer — "you can see assignments
    /// and none use this role". Rendering an omitted key as `0` would tell
    /// the operator a role is unused when the truth is that they cannot see.
    ///
    /// Display-only: not filterable, not orderable, and rejected in request
    /// bodies. Not transactionally consistent with the row it accompanies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment_count: Option<u64>,
}

/// `GET /rbac/v1/role-definitions/summary` response — how many roles of each
/// kind the caller can see in the catalog.
///
/// All three fields are unconditionally present: the UI renders the numbers
/// verbatim, so an empty bucket must reach the wire as `0` rather than as an
/// absent key that renders blank. Hence **no** `skip_serializing_if` here,
/// unlike [`RoleDefinitionDto::assignment_count`] where absence carries
/// meaning.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RoleDefinitionSummaryDto {
    /// Platform-seeded built-in roles visible to the caller. Built-ins are
    /// visible to every authenticated caller, so this is the whole shared
    /// catalog.
    pub built_in: u64,
    /// Tenant-owned custom roles the caller may read.
    pub custom: u64,
    /// `built_in + custom`. Derived, never queried separately, so it cannot
    /// disagree with its own parts.
    pub total: u64,
}

impl From<RoleTypeCounts> for RoleDefinitionSummaryDto {
    fn from(counts: RoleTypeCounts) -> Self {
        Self {
            built_in: counts.built_in,
            custom: counts.custom,
            total: counts.total(),
        }
    }
}

impl From<RoleDefinition> for RoleDefinitionDto {
    fn from(role: RoleDefinition) -> Self {
        Self {
            id: role.id,
            name: role.name,
            description: role.description,
            is_built_in: role.is_built_in,
            permissions: role.permissions.into_iter().map(Into::into).collect(),
            not_permissions: role.not_permissions.into_iter().map(Into::into).collect(),
            assignable_scopes: role
                .assignable_scopes
                .iter()
                .map(rbac_sdk::models::Scope::path)
                .collect(),
            owner_tenant_id: role.owner_tenant_id,
            created_at: role.created_at,
            updated_at: role.updated_at,
            created_by: role.created_by,
            assignment_count: role.assignment_count,
        }
    }
}

/// `POST /rbac/v1/role-definitions` request body.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateRoleDefinitionRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Allow rules. Accepts an empty array so callers can create
    /// deny-only roles (kept symmetrical with `not_permissions`).
    #[serde(default)]
    pub permissions: Vec<PermissionRuleDto>,
    /// Deny rules. Subtractive overlay applied after `permissions`.
    #[serde(default)]
    pub not_permissions: Vec<PermissionRuleDto>,
    pub assignable_scopes: Vec<String>,
    #[serde(default)]
    #[schema(value_type = Option<String>, format = "uuid")]
    pub owner_tenant_id: Option<Uuid>,
}

/// `PATCH /rbac/v1/role-definitions/{id}` request body. Immutable fields
/// (`id`, `is_built_in`, `owner_tenant_id`, `created_at`, `created_by`)
/// are accepted but trigger an `ImmutableFieldRejected` error at the
/// handler — they appear in the schema so clients receive a precise
/// 4xx instead of a silent ignore when they're sent.
///
/// The `description` field uses a double `Option` so the wire can
/// distinguish "unchanged" (`null` absent), "clear" (`null`), and
/// "set to a new value".
///
/// Immutable fields are typed (`Option<Uuid>`/`Option<bool>`/
/// `Option<DateTime<Utc>>`/`Option<String>`) so the `OpenAPI` schema
/// reflects the real shape (`string<uuid>`, `boolean`, etc.) and a
/// client sending a wrong type gets a typed deserialisation error
/// rather than a generic "field is immutable" — presence still
/// triggers `ImmutableFieldRejected` via `first_immutable_field`.
#[derive(Debug, Clone, Default)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PatchRoleDefinitionRequest {
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default)]
    pub is_built_in: Option<bool>,
    #[serde(default)]
    pub owner_tenant_id: Option<Uuid>,
    #[serde(default)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    #[allow(clippy::option_option)]
    pub description: Option<Option<String>>,
    /// Allow rules — `None` leaves the column unchanged; `Some(vec)`
    /// replaces it wholesale (including with an empty vec).
    #[serde(default)]
    pub permissions: Option<Vec<PermissionRuleDto>>,
    /// Deny rules — same `None` = unchanged, `Some(vec)` = replace
    /// semantics as `permissions`.
    #[serde(default)]
    pub not_permissions: Option<Vec<PermissionRuleDto>>,
    #[serde(default)]
    pub assignable_scopes: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Role-assignment DTOs
// ---------------------------------------------------------------------------

/// REST mirror of [`RoleAssignment`]. `scope` is rendered through
/// [`Scope::path`] so the wire shape (`"/"`, `"/tenants/{uuid}"`,
/// `"/tenants/{uuid}/resourceGroups/{uuid}"`) is preserved exactly.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RoleAssignmentDto {
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    #[schema(value_type = String, format = "uuid")]
    pub role_definition_id: Uuid,
    pub principal_id: String,
    pub principal_type: PrincipalTypeDto,
    pub scope: String,
    #[schema(value_type = String, format = "date-time")]
    pub created_at: DateTime<Utc>,
    #[schema(value_type = String, format = "date-time")]
    pub updated_at: DateTime<Utc>,
    pub created_by: String,
    /// Display name of the principal, resolved on the read path. The key
    /// is **omitted** (never `null`, never `""`) when the name could not
    /// be resolved — a deleted principal, a service principal, a
    /// principal outside the lookup tenant, a caller without user-read,
    /// or an unavailable upstream. Display-only: not filterable, not
    /// orderable, and rejected in request bodies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal_name: Option<String>,
    /// Display name of the author. Same omission semantics as
    /// [`Self::principal_name`]; additionally omitted for a machine
    /// author and for rows created before the author's kind was stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_name: Option<String>,
    /// Display name of the granted role definition, read from RBAC's own
    /// table while the response is rendered. Same omission semantics as
    /// [`Self::principal_name`], and omitted for the same reason a
    /// consumer should not special-case it: the only ways it can be
    /// missing are a definition deleted inside the FK-restrict race
    /// window and a failed local read. Display-only, exactly like the
    /// other two.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role_definition_name: Option<String>,
}

impl TryFrom<RoleAssignment> for RoleAssignmentDto {
    type Error = RbacServiceError;

    fn try_from(row: RoleAssignment) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            role_definition_id: row.role_definition_id,
            principal_id: row.principal_id,
            principal_type: row.principal_type.try_into()?,
            scope: scope_to_string(&row.scope),
            created_at: row.created_at,
            updated_at: row.updated_at,
            created_by: row.created_by,
            principal_name: row.principal_name,
            created_by_name: row.created_by_name,
            role_definition_name: row.role_definition_name,
        })
    }
}

fn scope_to_string(scope: &Scope) -> String {
    scope.path()
}

/// `POST /rbac/v1/role-assignments` request body. `principal_type` is a
/// raw string so unknown values produce a typed 400 `InvalidPrincipalType`
/// instead of a generic serde error; the schema still documents it as
/// `PrincipalTypeDto` for caller-facing clarity.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateRoleAssignmentRequest {
    #[schema(value_type = String, format = "uuid")]
    pub role_definition_id: Uuid,
    pub principal_id: String,
    #[schema(value_type = PrincipalTypeDto)]
    pub principal_type: String,
    pub scope: String,
}

// ---------------------------------------------------------------------------
// Permissions DTOs
// ---------------------------------------------------------------------------

/// REST mirror of [`AuthzPermission`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AuthzPermissionDto {
    pub id: String,
    pub resource_type: String,
    pub action: String,
    pub display_name: String,
}

impl From<AuthzPermission> for AuthzPermissionDto {
    fn from(p: AuthzPermission) -> Self {
        Self {
            id: p.id,
            resource_type: p.resource_type,
            action: p.action,
            display_name: p.display_name,
        }
    }
}
