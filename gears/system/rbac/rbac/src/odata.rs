//! `OData` filter field definitions for RBAC list endpoints.
//!
//! These enums declare which wire-named fields are valid in `$filter`
//! / `$orderby` clauses on RBAC list endpoints. They feed both the
//! OpenAPI `with_odata_filter::<F>()` helper and the `paginate_odata`
//! call in the repo layer. Column mappings live in
//! [`crate::infra::storage::odata_mapping`].
//!
//! The role-definition and role-assignment list endpoints drive
//! `paginate_odata` with these enums; `GET /rbac/v1/permissions` still
//! paginates by hand and declares no filter fields.

#![allow(dead_code)]

use toolkit_odata::filter::{FieldKind, FilterField};

/// Filter field enum for `GET /rbac/v1/role-assignments`.
///
/// Each variant maps 1:1 to a wire-visible field name (see [`Self::name`]).
/// `principal_id`, `principal_type`, `role_definition_id` and `scope` are
/// filtered with `$filter … eq`; a descendant match on scope is expressed as
/// `(scope eq 'X' or startswith(scope, 'X/'))`.
///
/// Two id fields, two [`FieldKind`]s, on purpose. `principal_id` is
/// `String` because the column is `text` holding an opaque provider-issued
/// id, and because `contains` / `startswith` / `endswith` are only offered
/// on string fields. `role_definition_id` is `Uuid` because the column is
/// `uuid` and a text parameter bound against it is a database-level type
/// error, not a filter that matches nothing.
///
/// A caller should not have to know that. Both spellings — `'<uuid>'`
/// quoted and `<uuid>` bare — are accepted for both fields, because
/// `infra::odata_normalize::normalize_filter_literals` coerces each literal
/// into the kind its field declares before the filter is validated. The
/// declared kinds here stay the ones the *column* needs.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum RoleAssignmentFilterField {
    Id,
    /// Principal subject id, as persisted in the `principal_id` **text**
    /// column — a UUID for `Group`, an opaque provider-issued string for
    /// `User` and `ServicePrincipal`.
    ///
    /// Declared `FieldKind::String` so substring predicates stay available
    /// (`contains` / `startswith` / `endswith` are string-only), and because
    /// the column is text.
    ///
    /// Caveat worth stating rather than discovering: a bare-uuid literal is
    /// normalized to its canonical hyphenated lowercase spelling, so a row
    /// whose stored text is *not* canonical (upper case, braces, no hyphens)
    /// is not matched by it. That is how an opaque text column has always
    /// behaved — `principal_id eq 'ABC…'` never matched `abc…` either — and
    /// the normalization neither introduces nor worsens it. A caller holding
    /// a non-canonical id can still quote it verbatim and match exactly.
    PrincipalId,
    PrincipalType,
    /// FK to `role_definitions.id`, a `uuid` column, hence
    /// `FieldKind::Uuid`: a text parameter bound against a `uuid` column is
    /// a database type error rather than a filter that matches nothing.
    /// Accepts the quoted spelling too — see the type note on the enum.
    RoleDefinitionId,
    Scope,
    /// Insertion timestamp — the default keyset order column
    /// (`created_at DESC, id DESC`, backed by
    /// `idx_role_assignments_created_at_id`). Must be a recognised
    /// field so the repo's default-order injection resolves via
    /// `FilterField::from_name`.
    CreatedAt,
    /// Author subject id, as persisted in the `created_by` text column.
    ///
    /// "Who granted these roles?" is an audit question the response has
    /// always answered and the query surface never did, which left the
    /// only way to ask it a full-table client-side scan.
    ///
    /// Being a recognised field makes it usable in `$orderby` as well as
    /// `$filter` — the two share this enum, and there is no per-field
    /// opt-out. Say the cost plainly rather than implying it is free: the
    /// column carries **no index**, so on a large `role_assignments` table
    /// an `eq` filter is a sequential scan and a keyset page ordered by it
    /// is a sort of the matching rows. That is acceptable for an audit
    /// query and would not be for a hot path; if it becomes one, the fix is
    /// a `(created_by, id)` index, not removing the field.
    ///
    /// [`Self::PrincipalId`]'s opacity caveat applies here too: `created_by`
    /// is a text column holding whatever subject id the identity provider
    /// issued, so a filter matches bytes, not identities.
    CreatedBy,
}

impl FilterField for RoleAssignmentFilterField {
    const FIELDS: &'static [Self] = &[
        Self::Id,
        Self::PrincipalId,
        Self::PrincipalType,
        Self::RoleDefinitionId,
        Self::Scope,
        Self::CreatedAt,
        Self::CreatedBy,
    ];

    fn name(&self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::PrincipalId => "principal_id",
            Self::PrincipalType => "principal_type",
            Self::RoleDefinitionId => "role_definition_id",
            Self::Scope => "scope",
            Self::CreatedAt => "created_at",
            Self::CreatedBy => "created_by",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::PrincipalId | Self::PrincipalType | Self::Scope | Self::CreatedBy => {
                FieldKind::String
            }
            Self::Id | Self::RoleDefinitionId => FieldKind::Uuid,
            Self::CreatedAt => FieldKind::DateTimeUtc,
        }
    }
}

/// Filter field enum for `GET /rbac/v1/role-definitions`.
///
/// Filtering is canonical `OData` `$filter` over these fields —
/// `is_built_in eq (true|false)`, `owner_tenant_id eq 'UUID'`, and
/// `contains(name, 'substring')`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum RoleDefinitionFilterField {
    Id,
    IsBuiltIn,
    OwnerTenantId,
    Name,
    /// Insertion timestamp — the default keyset order column
    /// (`created_at DESC, id DESC`, backed by
    /// `idx_role_definitions_created_at_id`). Required so the repo's
    /// default-order injection resolves via `FilterField::from_name`;
    /// without it the default list errored "Unknown orderby field"
    ///.
    CreatedAt,
}

impl FilterField for RoleDefinitionFilterField {
    const FIELDS: &'static [Self] = &[
        Self::Id,
        Self::IsBuiltIn,
        Self::OwnerTenantId,
        Self::Name,
        Self::CreatedAt,
    ];

    fn name(&self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::IsBuiltIn => "is_built_in",
            Self::OwnerTenantId => "owner_tenant_id",
            Self::Name => "name",
            Self::CreatedAt => "created_at",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::Name => FieldKind::String,
            Self::Id | Self::OwnerTenantId => FieldKind::Uuid,
            Self::IsBuiltIn => FieldKind::Bool,
            Self::CreatedAt => FieldKind::DateTimeUtc,
        }
    }
}
