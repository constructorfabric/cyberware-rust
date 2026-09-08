#![allow(clippy::expect_used)]
#![cfg(all(feature = "integration", feature = "pg"))]

//! `PostgreSQL` execution coverage for native resource-group scope predicates.
//!
//! The membership table stores external resource identifiers as `TEXT`, while
//! domain entities commonly use native `UUID` primary keys. These tests execute
//! the complete `SecureORM` condition against `PostgreSQL` because debug-tree
//! tests cannot detect `PostgreSQL`'s `uuid = text` operator error. They also pin the
//! membership-type discriminator: the same textual identifier may legitimately
//! occur under multiple RG member-handle types.

mod common;

use anyhow::Result;
use sea_orm::Set;
use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude as mig;
use sea_orm_migration::prelude::Iden;
use sea_orm_migration::sea_query;
use sea_orm_migration::sea_query::IntoIden;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{ScopableEntity, SecureEntityExt, secure_insert};
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, ScopeValue, pep_properties};
use uuid::Uuid;

const RESOURCE_MEMBER_TYPE: &str = "gts.cf.core.rg.type.v1~example.core.rg.resource.v1~";
const OTHER_MEMBER_TYPE: &str = "gts.cf.core.rg.type.v1~example.core.rg.other.v1~";

#[derive(Iden)]
enum ResourceTable {
    #[iden = "group_scope_resource"]
    Table,
    Id,
    TenantId,
    Name,
}

#[derive(Iden)]
enum GtsTypeTable {
    #[iden = "gts_type"]
    Table,
    Id,
    SchemaId,
}

#[derive(Iden)]
enum MembershipTable {
    #[iden = "resource_group_membership"]
    Table,
    GroupId,
    GtsTypeId,
    ResourceId,
}

#[derive(Iden)]
enum ClosureTable {
    #[iden = "resource_group_closure"]
    Table,
    AncestorId,
    DescendantId,
}

/// Minimal production-shaped schema needed by `InGroup` and
/// `InGroupSubtree`. DDL stays in migration infrastructure, matching the
/// toolkit's no-raw-SQL test policy.
struct CreateGroupScopeSchema;

impl mig::MigrationName for CreateGroupScopeSchema {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "m001_create_group_scope_schema"
    }
}

#[async_trait::async_trait]
impl mig::MigrationTrait for CreateGroupScopeSchema {
    async fn up(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .create_table(
                mig::Table::create()
                    .table(ResourceTable::Table)
                    .col(
                        mig::ColumnDef::new(ResourceTable::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        mig::ColumnDef::new(ResourceTable::TenantId)
                            .uuid()
                            .not_null(),
                    )
                    .col(mig::ColumnDef::new(ResourceTable::Name).text().not_null())
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                mig::Table::create()
                    .table(GtsTypeTable::Table)
                    .col(
                        mig::ColumnDef::new(GtsTypeTable::Id)
                            .small_integer()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        mig::ColumnDef::new(GtsTypeTable::SchemaId)
                            .text()
                            .not_null()
                            .unique_key(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                mig::Table::create()
                    .table(MembershipTable::Table)
                    .col(
                        mig::ColumnDef::new(MembershipTable::GroupId)
                            .uuid()
                            .not_null(),
                    )
                    .col(
                        mig::ColumnDef::new(MembershipTable::GtsTypeId)
                            .small_integer()
                            .not_null(),
                    )
                    .col(
                        mig::ColumnDef::new(MembershipTable::ResourceId)
                            .text()
                            .not_null(),
                    )
                    .primary_key(
                        mig::Index::create()
                            .col(MembershipTable::GroupId)
                            .col(MembershipTable::GtsTypeId)
                            .col(MembershipTable::ResourceId),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                mig::Table::create()
                    .table(ClosureTable::Table)
                    .col(
                        mig::ColumnDef::new(ClosureTable::AncestorId)
                            .uuid()
                            .not_null(),
                    )
                    .col(
                        mig::ColumnDef::new(ClosureTable::DescendantId)
                            .uuid()
                            .not_null(),
                    )
                    .primary_key(
                        mig::Index::create()
                            .col(ClosureTable::AncestorId)
                            .col(ClosureTable::DescendantId),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        for table in [
            ClosureTable::Table.into_iden(),
            MembershipTable::Table.into_iden(),
            GtsTypeTable::Table.into_iden(),
            ResourceTable::Table.into_iden(),
        ] {
            manager
                .drop_table(mig::Table::drop().table(table).to_owned())
                .await?;
        }
        Ok(())
    }
}

mod resource {
    use sea_orm::entity::prelude::*;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "group_scope_resource")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod gts_type {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "gts_type")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i16,
        pub schema_id: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod membership {
    use sea_orm::entity::prelude::*;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "resource_group_membership")]
    // Column names intentionally mirror the canonical RG composite key.
    #[allow(clippy::struct_field_names)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub group_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub gts_type_id: i16,
        #[sea_orm(primary_key, auto_increment = false)]
        pub resource_id: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod closure {
    use sea_orm::entity::prelude::*;
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "resource_group_closure")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub ancestor_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub descendant_id: Uuid,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

impl ScopableEntity for resource::Entity {
    fn tenant_col() -> Option<resource::Column> {
        Some(resource::Column::TenantId)
    }

    fn resource_col() -> Option<resource::Column> {
        Some(resource::Column::Id)
    }

    fn owner_col() -> Option<resource::Column> {
        None
    }

    fn type_col() -> Option<resource::Column> {
        None
    }

    fn resolve_property(property: &str) -> Option<resource::Column> {
        match property {
            p if p == pep_properties::OWNER_TENANT_ID => Self::tenant_col(),
            p if p == pep_properties::RESOURCE_ID => Self::resource_col(),
            _ => None,
        }
    }
}

macro_rules! unrestricted_entity {
    ($entity:path, $column:path) => {
        impl ScopableEntity for $entity {
            const IS_UNRESTRICTED: bool = true;

            fn tenant_col() -> Option<$column> {
                None
            }

            fn resource_col() -> Option<$column> {
                None
            }

            fn owner_col() -> Option<$column> {
                None
            }

            fn type_col() -> Option<$column> {
                None
            }

            fn resolve_property(_property: &str) -> Option<$column> {
                None
            }
        }
    };
}

unrestricted_entity!(gts_type::Entity, gts_type::Column);
unrestricted_entity!(membership::Entity, membership::Column);
unrestricted_entity!(closure::Entity, closure::Column);

async fn setup() -> Result<(common::DbUnderTest, toolkit_db::Db)> {
    let dut = common::bring_up_postgres().await?;
    let db = toolkit_db::connect_db(&dut.url, toolkit_db::ConnectOpts::default()).await?;
    run_migrations_for_testing(&db, vec![Box::new(CreateGroupScopeSchema)])
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok((dut, db))
}

async fn seed_type(conn: &toolkit_db::secure::DbConn<'_>, id: i16, schema_id: &str) -> Result<()> {
    secure_insert::<gts_type::Entity>(
        gts_type::ActiveModel {
            id: Set(id),
            schema_id: Set(schema_id.to_owned()),
        },
        &AccessScope::allow_all(),
        conn,
    )
    .await?;
    Ok(())
}

async fn seed_resource(
    conn: &toolkit_db::secure::DbConn<'_>,
    id: Uuid,
    tenant_id: Uuid,
    name: &str,
) -> Result<()> {
    secure_insert::<resource::Entity>(
        resource::ActiveModel {
            id: Set(id),
            tenant_id: Set(tenant_id),
            name: Set(name.to_owned()),
        },
        &AccessScope::allow_all(),
        conn,
    )
    .await?;
    Ok(())
}

async fn seed_membership(
    conn: &toolkit_db::secure::DbConn<'_>,
    group_id: Uuid,
    gts_type_id: i16,
    resource_id: impl Into<String>,
) -> Result<()> {
    secure_insert::<membership::Entity>(
        membership::ActiveModel {
            group_id: Set(group_id),
            gts_type_id: Set(gts_type_id),
            resource_id: Set(resource_id.into()),
        },
        &AccessScope::allow_all(),
        conn,
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn in_group_casts_uuid_to_text_and_excludes_other_member_types() -> Result<()> {
    let (_container, db) = setup().await?;
    let conn = db.conn()?;
    let tenant_id = Uuid::new_v4();
    let foreign_tenant_id = Uuid::new_v4();
    let group_id = Uuid::new_v4();
    let matching_id = Uuid::new_v4();
    let wrong_type_id = Uuid::new_v4();
    let foreign_tenant_resource_id = Uuid::new_v4();

    seed_type(&conn, 1, RESOURCE_MEMBER_TYPE).await?;
    seed_type(&conn, 2, OTHER_MEMBER_TYPE).await?;
    seed_resource(&conn, matching_id, tenant_id, "matching").await?;
    seed_resource(&conn, wrong_type_id, tenant_id, "wrong-type").await?;
    seed_resource(
        &conn,
        foreign_tenant_resource_id,
        foreign_tenant_id,
        "foreign-tenant",
    )
    .await?;
    seed_membership(&conn, group_id, 1, matching_id.to_string()).await?;
    // Even a correctly typed membership cannot escape the mandatory tenant
    // predicate carried in the same AND constraint.
    seed_membership(&conn, group_id, 1, foreign_tenant_resource_id.to_string()).await?;
    // External IDs are unique only within an RG member-handle type, so the same
    // text may legitimately occur under another type.
    seed_membership(&conn, group_id, 2, matching_id.to_string()).await?;
    seed_membership(&conn, group_id, 2, wrong_type_id.to_string()).await?;
    // RG does not validate ID syntax per member type. Even a selected type can
    // contain an opaque ID, so casting the membership side to UUID would make
    // the PostgreSQL query throw before it can return the UUID-backed entity.
    seed_membership(&conn, group_id, 1, "opaque-resource-id").await?;

    let scope = AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
        ScopeFilter::in_group_typed(
            pep_properties::RESOURCE_ID,
            RESOURCE_MEMBER_TYPE,
            vec![ScopeValue::Uuid(group_id)],
        ),
    ]));
    let rows = resource::Entity::find()
        .secure()
        .scope_with(&scope)
        .all(&conn)
        .await?;

    assert_eq!(rows.len(), 1, "only the correctly typed member must match");
    assert_eq!(rows[0].id, matching_id);
    assert_eq!(rows[0].name, "matching");

    let unknown_type_scope = AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
        ScopeFilter::in_group_typed(
            pep_properties::RESOURCE_ID,
            "gts.cf.core.rg.type.v1~example.core.rg.missing.v1~",
            vec![ScopeValue::Uuid(group_id)],
        ),
    ]));
    let unknown_type_rows = resource::Entity::find()
        .secure()
        .scope_with(&unknown_type_scope)
        .all(&conn)
        .await?;
    assert!(
        unknown_type_rows.is_empty(),
        "an unknown external member type must match no membership rows"
    );
    Ok(())
}

#[tokio::test]
async fn in_group_subtree_matches_descendants_but_not_unrelated_groups() -> Result<()> {
    let (_container, db) = setup().await?;
    let conn = db.conn()?;
    let tenant_id = Uuid::new_v4();
    let ancestor_id = Uuid::new_v4();
    let descendant_id = Uuid::new_v4();
    let unrelated_id = Uuid::new_v4();
    let direct_resource = Uuid::new_v4();
    let descendant_resource = Uuid::new_v4();
    let wrong_type_resource = Uuid::new_v4();
    let unrelated_resource = Uuid::new_v4();

    seed_type(&conn, 1, RESOURCE_MEMBER_TYPE).await?;
    seed_type(&conn, 2, OTHER_MEMBER_TYPE).await?;
    seed_resource(&conn, direct_resource, tenant_id, "direct").await?;
    seed_resource(&conn, descendant_resource, tenant_id, "descendant").await?;
    seed_resource(&conn, wrong_type_resource, tenant_id, "wrong-type").await?;
    seed_resource(&conn, unrelated_resource, tenant_id, "unrelated").await?;
    // The closure self-row makes direct members of the selected ancestor part
    // of its subtree, just as it does in the canonical RG closure table.
    seed_membership(&conn, ancestor_id, 1, direct_resource.to_string()).await?;
    seed_membership(&conn, descendant_id, 1, descendant_resource.to_string()).await?;
    // Duplicate external IDs across types are valid, while a row belonging
    // only to the wrong type must never grant access.
    seed_membership(&conn, descendant_id, 2, descendant_resource.to_string()).await?;
    seed_membership(&conn, descendant_id, 2, wrong_type_resource.to_string()).await?;
    // A selected-type opaque ID proves the query never casts membership IDs.
    seed_membership(&conn, descendant_id, 1, "opaque-descendant-id").await?;
    seed_membership(&conn, unrelated_id, 1, unrelated_resource.to_string()).await?;
    for descendant_id in [ancestor_id, descendant_id] {
        secure_insert::<closure::Entity>(
            closure::ActiveModel {
                ancestor_id: Set(ancestor_id),
                descendant_id: Set(descendant_id),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await?;
    }

    let scope = AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
        ScopeFilter::in_group_subtree_typed(
            pep_properties::RESOURCE_ID,
            RESOURCE_MEMBER_TYPE,
            vec![ScopeValue::Uuid(ancestor_id)],
        ),
    ]));
    let rows = resource::Entity::find()
        .secure()
        .scope_with(&scope)
        .all(&conn)
        .await?;

    let mut names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["descendant", "direct"],
        "the subtree must include direct and descendant members while excluding unrelated groups and member types"
    );
    Ok(())
}
