//! `OData` field → `SeaORM` column mappers for RBAC list endpoints.
//!
//! `paginate_odata::<F, M, _, _, _, _>(...)` is generic over a filter
//! field type `F` and a mapper type `M: ODataFieldMapping<F>`; this
//! module supplies the mappers `M`.
//!
//! Status: the mapper structs are wired up but the list-endpoint
//! migration to `toolkit_odata` has not yet landed in the handler /
//! service / repo layer. `#[allow(dead_code)]` keeps the foundation
//! quiet until the migration PR consumes these.

#![allow(dead_code)]

use toolkit_db::odata::sea_orm_filter::{FieldToColumn, ODataFieldMapping};

use crate::infra::storage::entity::role_assignment::{
    Column as RoleAssignmentColumn, Entity as RoleAssignmentEntity, Model as RoleAssignmentModel,
};
use crate::infra::storage::entity::role_definition::{
    Column as RoleDefinitionColumn, Entity as RoleDefinitionEntity, Model as RoleDefinitionModel,
};
use crate::odata::{RoleAssignmentFilterField, RoleDefinitionFilterField};

/// Maps [`RoleAssignmentFilterField`] variants to their backing
/// [`role_assignment`](crate::infra::storage::entity::role_assignment)
/// columns and supplies the seekset cursor-value extractor.
pub struct RoleAssignmentODataMapper;

impl FieldToColumn<RoleAssignmentFilterField> for RoleAssignmentODataMapper {
    type Column = RoleAssignmentColumn;

    fn map_field(field: RoleAssignmentFilterField) -> RoleAssignmentColumn {
        match field {
            RoleAssignmentFilterField::Id => RoleAssignmentColumn::Id,
            RoleAssignmentFilterField::PrincipalId => RoleAssignmentColumn::PrincipalId,
            RoleAssignmentFilterField::PrincipalType => RoleAssignmentColumn::PrincipalType,
            RoleAssignmentFilterField::RoleDefinitionId => RoleAssignmentColumn::RoleDefinitionId,
            RoleAssignmentFilterField::Scope => RoleAssignmentColumn::Scope,
            RoleAssignmentFilterField::CreatedAt => RoleAssignmentColumn::CreatedAt,
            RoleAssignmentFilterField::CreatedBy => RoleAssignmentColumn::CreatedBy,
        }
    }
}

impl ODataFieldMapping<RoleAssignmentFilterField> for RoleAssignmentODataMapper {
    type Entity = RoleAssignmentEntity;

    fn extract_cursor_value(
        model: &RoleAssignmentModel,
        field: RoleAssignmentFilterField,
    ) -> sea_orm::Value {
        match field {
            RoleAssignmentFilterField::Id => sea_orm::Value::Uuid(Some(model.id)),
            RoleAssignmentFilterField::PrincipalId => {
                sea_orm::Value::String(Some(model.principal_id.clone()))
            }
            RoleAssignmentFilterField::PrincipalType => {
                sea_orm::Value::String(Some(model.principal_type.clone()))
            }
            RoleAssignmentFilterField::RoleDefinitionId => {
                sea_orm::Value::Uuid(Some(model.role_definition_id))
            }
            RoleAssignmentFilterField::Scope => sea_orm::Value::String(Some(model.scope.clone())),
            RoleAssignmentFilterField::CreatedAt => {
                sea_orm::Value::ChronoDateTimeUtc(Some(model.created_at))
            }
            // `created_by` is orderable because filter fields and orderby
            // fields share one enum, so the seekset needs a cursor value for
            // it. `text` column → `Value::String`, matching how
            // `build_binary_condition` binds a string literal.
            RoleAssignmentFilterField::CreatedBy => {
                sea_orm::Value::String(Some(model.created_by.clone()))
            }
        }
    }
}

/// Maps [`RoleDefinitionFilterField`] variants to their backing
/// [`role_definition`](crate::infra::storage::entity::role_definition)
/// columns and supplies the seekset cursor-value extractor.
pub struct RoleDefinitionODataMapper;

impl FieldToColumn<RoleDefinitionFilterField> for RoleDefinitionODataMapper {
    type Column = RoleDefinitionColumn;

    fn map_field(field: RoleDefinitionFilterField) -> RoleDefinitionColumn {
        match field {
            RoleDefinitionFilterField::Id => RoleDefinitionColumn::Id,
            RoleDefinitionFilterField::IsBuiltIn => RoleDefinitionColumn::IsBuiltIn,
            RoleDefinitionFilterField::OwnerTenantId => RoleDefinitionColumn::OwnerTenantId,
            RoleDefinitionFilterField::Name => RoleDefinitionColumn::Name,
            RoleDefinitionFilterField::CreatedAt => RoleDefinitionColumn::CreatedAt,
        }
    }
}

impl ODataFieldMapping<RoleDefinitionFilterField> for RoleDefinitionODataMapper {
    type Entity = RoleDefinitionEntity;

    fn extract_cursor_value(
        model: &RoleDefinitionModel,
        field: RoleDefinitionFilterField,
    ) -> sea_orm::Value {
        match field {
            RoleDefinitionFilterField::Id => sea_orm::Value::Uuid(Some(model.id)),
            RoleDefinitionFilterField::IsBuiltIn => sea_orm::Value::Bool(Some(model.is_built_in)),
            RoleDefinitionFilterField::OwnerTenantId => match model.owner_tenant_id {
                Some(uuid) => sea_orm::Value::Uuid(Some(uuid)),
                None => sea_orm::Value::Uuid(None),
            },
            RoleDefinitionFilterField::Name => sea_orm::Value::String(Some(model.name.clone())),
            RoleDefinitionFilterField::CreatedAt => {
                sea_orm::Value::ChronoDateTimeUtc(Some(model.created_at))
            }
        }
    }
}
