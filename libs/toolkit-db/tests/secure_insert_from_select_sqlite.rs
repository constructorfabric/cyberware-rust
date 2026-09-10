#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(feature = "sqlite")]

//! `secure_insert_from_select` needs a real runner to reach the scope check
//! at all, so this can't live as a sync unit test next to
//! `insert_from_select_allowed`'s tests in `db_ops.rs` -- follows the same
//! `SQLite`-backed pattern as `secure_odata_sqlite.rs`.

use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude as mig;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{ScopableEntity, ScopeError, secure_insert_from_select};
use toolkit_db::{ConnectOpts, connect_db};
use toolkit_security::AccessScope;

// A link table with no scope columns -- the shape `secure_insert_from_select`
// is meant for, mirroring `resource_group_closure`.
mod link {
    use sea_orm::entity::prelude::*;

    #[derive(Debug, Clone, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "insert_from_select_link")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub left_id: i64,
        #[sea_orm(primary_key, auto_increment = false)]
        pub right_id: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

impl ScopableEntity for link::Entity {
    fn tenant_col() -> Option<<Self as EntityTrait>::Column> {
        None
    }
    fn resource_col() -> Option<<Self as EntityTrait>::Column> {
        None
    }
    fn owner_col() -> Option<<Self as EntityTrait>::Column> {
        None
    }
    fn type_col() -> Option<<Self as EntityTrait>::Column> {
        None
    }
    fn resolve_property(_property: &str) -> Option<<Self as EntityTrait>::Column> {
        None
    }
    fn scope_columns() -> Vec<<Self as EntityTrait>::Column> {
        Vec::new()
    }
}

struct CreateLinkTable;

impl mig::MigrationName for CreateLinkTable {
    fn name(&self) -> &'static str {
        "m001_create_insert_from_select_link"
    }
}

#[async_trait::async_trait]
impl mig::MigrationTrait for CreateLinkTable {
    async fn up(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .create_table(
                mig::Table::create()
                    .table(mig::Alias::new("insert_from_select_link"))
                    .if_not_exists()
                    .col(
                        mig::ColumnDef::new(mig::Alias::new("left_id"))
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        mig::ColumnDef::new(mig::Alias::new("right_id"))
                            .big_integer()
                            .not_null(),
                    )
                    .primary_key(
                        mig::Index::create()
                            .col(mig::Alias::new("left_id"))
                            .col(mig::Alias::new("right_id")),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .drop_table(
                mig::Table::drop()
                    .table(mig::Alias::new("insert_from_select_link"))
                    .to_owned(),
            )
            .await
    }
}

#[tokio::test]
async fn insert_from_select_denies_a_constrained_scope() {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("db connect");
    run_migrations_for_testing(&db, vec![Box::new(CreateLinkTable)])
        .await
        .expect("migrate");
    let conn = db.conn().expect("conn");

    // Any scope short of unconstrained must be refused before the statement
    // is even built -- deny-all is the strictest instance of "not
    // unconstrained", so it stands in for the whole class here.
    let deny_all = AccessScope::deny_all();

    let source = sea_orm::sea_query::Query::select()
        .expr(sea_orm::sea_query::Expr::val(1_i64))
        .expr(sea_orm::sea_query::Expr::val(2_i64))
        .to_owned();

    let err = secure_insert_from_select::<link::Entity, _>(
        [link::Column::LeftId, link::Column::RightId],
        source,
        &deny_all,
        &conn,
    )
    .await
    .expect_err("a constrained scope must be denied");

    assert!(matches!(err, ScopeError::Denied(_)), "got: {err:?}");
}
