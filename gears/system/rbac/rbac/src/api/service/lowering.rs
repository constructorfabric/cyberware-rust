//! Domain Model → SDK projection helpers.
//!
//! The single place where [`CountedRoleDefinition`] /
//! [`HydratedRoleAssignment`] become [`rbac_sdk::models::RoleDefinition`] /
//! [`rbac_sdk::models::RoleAssignment`]. Every api/service handler that
//! returns an SDK type funnels through here so the lowering rule lives
//! in exactly one place — mirrors the canonical tenant lowering at
//! `account-management/src/domain/tenant/service/mod.rs::lower_to_tenant`.
//!
//! Both aggregates are lowered from a read-path *view* rather than from the
//! bare row projection: display names and the assignment count are resolved
//! at the service boundary, so the row projections themselves stay free of
//! them.

use rbac_sdk::models::{RoleAssignment, RoleDefinition};

use crate::domain::role_assignment::HydratedRoleAssignment;
use crate::domain::role_definition::CountedRoleDefinition;

/// Lower a [`CountedRoleDefinition`] to its SDK aggregate.
///
/// Every field moves across unchanged: domain and SDK both hold typed
/// [`rbac_sdk::models::Scope`] values for `assignable_scopes`, so this
/// conversion has no
/// re-encoding step left.
///
/// Takes the counted *view* rather than the bare row projection, for the
/// same reason the assignment lowering takes the hydrated view: the
/// assignment count is resolved at the service boundary and must not be
/// smuggled onto `RoleDefinitionModel`, which is a row projection. Write
/// paths lower a `CountedRoleDefinition::bare(model)` and therefore emit no
/// count.
#[must_use]
pub fn lower_role_definition(counted: CountedRoleDefinition) -> RoleDefinition {
    let CountedRoleDefinition {
        model,
        assignment_count,
    } = counted;
    RoleDefinition::new(
        model.id,
        model.name,
        model.description,
        model.is_built_in,
        model.permissions,
        model.not_permissions,
        model.assignable_scopes,
        model.owner_tenant_id,
        model.created_at,
        model.updated_at,
        model.created_by,
    )
    .with_assignment_count(assignment_count)
}

/// Lower a [`HydratedRoleAssignment`] to its SDK aggregate. Every row
/// field has a one-to-one SDK counterpart, so that half is a straight
/// transcription; the three display names ride along through the SDK's
/// chainable setters, which forward `None` unchanged so an unresolved
/// name stays unresolved all the way to the wire.
///
/// The destructuring is exhaustive on purpose: a fourth resolved name
/// added to the view type fails to compile here rather than being
/// silently dropped on the way to the wire.
#[must_use]
pub fn lower_role_assignment(row: HydratedRoleAssignment) -> RoleAssignment {
    let HydratedRoleAssignment {
        model,
        principal_name,
        created_by_name,
        role_definition_name,
    } = row;
    RoleAssignment::new(
        model.id,
        model.role_definition_id,
        model.principal_id,
        model.principal_type,
        model.scope,
        model.created_at,
        model.updated_at,
        model.created_by,
    )
    .with_principal_name(principal_name)
    .with_created_by_name(created_by_name)
    .with_role_definition_name(role_definition_name)
}
