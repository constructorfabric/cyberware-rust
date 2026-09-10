#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for `SecureSelect::project_all`.
//!
//! Security contract:
//! - No raw SQL in tests.
//! - Schema is created via `sea-orm-migration` definitions executed by the migration runner.

use sea_orm::entity::prelude::*;
use sea_orm::sea_query::Expr;
use sea_orm::{ExprTrait, FromQueryResult, JoinType, QueryFilter, QuerySelect, RelationTrait, Set};
use sea_orm_migration::prelude as mig;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{Db, DbConn, ScopableEntity, SecureEntityExt, secure_insert};
use toolkit_db::{ConnectOpts, connect_db};
use toolkit_security::{AccessScope, pep_properties};
use uuid::Uuid;

// ════════════════════════════════════════════════════════════════════
// Test entities
// ════════════════════════════════════════════════════════════════════

mod order_ent {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "project_all_orders")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub category: String,
        pub amount: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(has_many = "super::item_ent::Entity")]
        Items,
    }

    impl Related<super::item_ent::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Items.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

impl ScopableEntity for order_ent::Entity {
    fn tenant_col() -> Option<<Self as EntityTrait>::Column> {
        Some(order_ent::Column::TenantId)
    }
    fn resource_col() -> Option<<Self as EntityTrait>::Column> {
        Some(order_ent::Column::Id)
    }
    fn owner_col() -> Option<<Self as EntityTrait>::Column> {
        None
    }
    fn type_col() -> Option<<Self as EntityTrait>::Column> {
        None
    }
    fn resolve_property(property: &str) -> Option<<Self as EntityTrait>::Column> {
        match property {
            p if p == pep_properties::OWNER_TENANT_ID => Self::tenant_col(),
            p if p == pep_properties::RESOURCE_ID => Self::resource_col(),
            _ => None,
        }
    }
    fn scope_columns() -> Vec<<Self as EntityTrait>::Column> {
        vec![order_ent::Column::TenantId, order_ent::Column::Id]
    }
}

mod item_ent {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "project_all_items")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub order_id: Uuid,
        pub qty: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::order_ent::Entity",
            from = "Column::OrderId",
            to = "super::order_ent::Column::Id"
        )]
        Order,
    }

    impl Related<super::order_ent::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Order.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

impl ScopableEntity for item_ent::Entity {
    fn tenant_col() -> Option<<Self as EntityTrait>::Column> {
        Some(item_ent::Column::TenantId)
    }
    fn resource_col() -> Option<<Self as EntityTrait>::Column> {
        Some(item_ent::Column::Id)
    }
    fn owner_col() -> Option<<Self as EntityTrait>::Column> {
        None
    }
    fn type_col() -> Option<<Self as EntityTrait>::Column> {
        None
    }
    fn resolve_property(property: &str) -> Option<<Self as EntityTrait>::Column> {
        match property {
            p if p == pep_properties::OWNER_TENANT_ID => Self::tenant_col(),
            p if p == pep_properties::RESOURCE_ID => Self::resource_col(),
            _ => None,
        }
    }
    fn scope_columns() -> Vec<<Self as EntityTrait>::Column> {
        vec![item_ent::Column::TenantId, item_ent::Column::Id]
    }
}

// ════════════════════════════════════════════════════════════════════
// Migration
// ════════════════════════════════════════════════════════════════════

struct CreateProjectAllTables;

impl mig::MigrationName for CreateProjectAllTables {
    fn name(&self) -> &'static str {
        "m001_create_project_all_tables"
    }
}

#[async_trait::async_trait]
impl mig::MigrationTrait for CreateProjectAllTables {
    async fn up(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .create_table(
                mig::Table::create()
                    .table(mig::Alias::new("project_all_orders"))
                    .if_not_exists()
                    .col(
                        mig::ColumnDef::new(mig::Alias::new("id"))
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        mig::ColumnDef::new(mig::Alias::new("tenant_id"))
                            .uuid()
                            .not_null(),
                    )
                    .col(
                        mig::ColumnDef::new(mig::Alias::new("category"))
                            .string()
                            .not_null(),
                    )
                    .col(
                        mig::ColumnDef::new(mig::Alias::new("amount"))
                            .big_integer()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                mig::Table::create()
                    .table(mig::Alias::new("project_all_items"))
                    .if_not_exists()
                    .col(
                        mig::ColumnDef::new(mig::Alias::new("id"))
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        mig::ColumnDef::new(mig::Alias::new("tenant_id"))
                            .uuid()
                            .not_null(),
                    )
                    .col(
                        mig::ColumnDef::new(mig::Alias::new("order_id"))
                            .uuid()
                            .not_null(),
                    )
                    .col(
                        mig::ColumnDef::new(mig::Alias::new("qty"))
                            .big_integer()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .drop_table(
                mig::Table::drop()
                    .table(mig::Alias::new("project_all_items"))
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(
                mig::Table::drop()
                    .table(mig::Alias::new("project_all_orders"))
                    .to_owned(),
            )
            .await
    }
}

// ════════════════════════════════════════════════════════════════════
// Test helpers
// ════════════════════════════════════════════════════════════════════

struct TestDb {
    db: Db,
}

impl TestDb {
    async fn new() -> Self {
        let test_id = Uuid::new_v4();
        let dsn = format!("sqlite:file:memdb_project_all_{test_id}?mode=memory&cache=shared");
        let opts = ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        };
        let db = connect_db(&dsn, opts).await.expect("db connect");

        run_migrations_for_testing(&db, vec![Box::new(CreateProjectAllTables)])
            .await
            .expect("migrate");

        Self { db }
    }

    fn conn(&self) -> DbConn<'_> {
        self.db.conn().expect("conn")
    }
}

async fn insert_order(
    conn: &DbConn<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    category: &str,
    amount: i64,
) -> Uuid {
    let id = Uuid::new_v4();
    let am = order_ent::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        category: Set(category.to_owned()),
        amount: Set(amount),
    };
    secure_insert::<order_ent::Entity>(am, scope, conn)
        .await
        .expect("insert order");
    id
}

async fn insert_item(
    conn: &DbConn<'_>,
    scope: &AccessScope,
    tenant_id: Uuid,
    order_id: Uuid,
    qty: i64,
) -> Uuid {
    let id = Uuid::new_v4();
    let am = item_ent::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        order_id: Set(order_id),
        qty: Set(qty),
    };
    secure_insert::<item_ent::Entity>(am, scope, conn)
        .await
        .expect("insert item");
    id
}

// ════════════════════════════════════════════════════════════════════
// Projection models
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, FromQueryResult)]
struct CategoryTotal {
    category: String,
    total: i64,
}

#[derive(Debug, FromQueryResult)]
struct OrderItemCount {
    order_id: Uuid,
    cnt: i64,
}

#[derive(Debug, FromQueryResult)]
struct SingleColumn {
    category: String,
}

// ════════════════════════════════════════════════════════════════════
// Tests: basic projection (GROUP BY + aggregate)
// ════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn project_all_group_by_aggregate() {
    let db = TestDb::new().await;
    let conn = db.conn();
    let tid = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tid);

    insert_order(&conn, &scope, tid, "books", 10).await;
    insert_order(&conn, &scope, tid, "books", 20).await;
    insert_order(&conn, &scope, tid, "electronics", 50).await;

    let mut rows: Vec<CategoryTotal> = order_ent::Entity::find()
        .secure()
        .scope_with(&scope)
        .project_all(&conn, |q| {
            q.select_only()
                .column(order_ent::Column::Category)
                .column_as(Expr::col(order_ent::Column::Amount).sum(), "total")
                .group_by(order_ent::Column::Category)
                .into_model::<CategoryTotal>()
        })
        .await
        .expect("project_all aggregate");

    rows.sort_by(|a, b| a.category.cmp(&b.category));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].category, "books");
    assert_eq!(rows[0].total, 30);
    assert_eq!(rows[1].category, "electronics");
    assert_eq!(rows[1].total, 50);
}

// ════════════════════════════════════════════════════════════════════
// Tests: tenant isolation
// ════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn project_all_cross_tenant_returns_empty() {
    let db = TestDb::new().await;
    let conn = db.conn();
    let tid_a = Uuid::new_v4();
    let tid_b = Uuid::new_v4();
    let scope_a = AccessScope::for_tenant(tid_a);
    let scope_b = AccessScope::for_tenant(tid_b);

    insert_order(&conn, &scope_a, tid_a, "books", 10).await;
    insert_order(&conn, &scope_a, tid_a, "books", 20).await;

    // Query with scope_b — must see nothing from tenant A
    let rows: Vec<CategoryTotal> = order_ent::Entity::find()
        .secure()
        .scope_with(&scope_b)
        .project_all(&conn, |q| {
            q.select_only()
                .column(order_ent::Column::Category)
                .column_as(Expr::col(order_ent::Column::Amount).sum(), "total")
                .group_by(order_ent::Column::Category)
                .into_model::<CategoryTotal>()
        })
        .await
        .expect("project_all cross-tenant");

    assert!(rows.is_empty(), "cross-tenant must return empty");
}

#[tokio::test]
async fn project_all_deny_all_scope_returns_empty() {
    let db = TestDb::new().await;
    let conn = db.conn();
    let tid = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tid);
    let deny_scope = AccessScope::default();

    insert_order(&conn, &scope, tid, "books", 10).await;

    let rows: Vec<SingleColumn> = order_ent::Entity::find()
        .secure()
        .scope_with(&deny_scope)
        .project_all(&conn, |q| {
            q.select_only()
                .column(order_ent::Column::Category)
                .into_model::<SingleColumn>()
        })
        .await
        .expect("project_all deny scope");

    assert!(rows.is_empty(), "deny-all scope must return empty");
}

// ════════════════════════════════════════════════════════════════════
// Tests: empty result
// ════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn project_all_no_data_returns_empty() {
    let db = TestDb::new().await;
    let conn = db.conn();
    let tid = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tid);

    let rows: Vec<CategoryTotal> = order_ent::Entity::find()
        .secure()
        .scope_with(&scope)
        .project_all(&conn, |q| {
            q.select_only()
                .column(order_ent::Column::Category)
                .column_as(Expr::col(order_ent::Column::Amount).sum(), "total")
                .group_by(order_ent::Column::Category)
                .into_model::<CategoryTotal>()
        })
        .await
        .expect("project_all empty");

    assert!(rows.is_empty());
}

// ════════════════════════════════════════════════════════════════════
// Tests: select_only without aggregation (column projection)
// ════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn project_all_select_only_columns() {
    let db = TestDb::new().await;
    let conn = db.conn();
    let tid = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tid);

    insert_order(&conn, &scope, tid, "books", 10).await;
    insert_order(&conn, &scope, tid, "electronics", 50).await;

    let mut rows: Vec<SingleColumn> = order_ent::Entity::find()
        .secure()
        .scope_with(&scope)
        .project_all(&conn, |q| {
            q.select_only()
                .column(order_ent::Column::Category)
                .into_model::<SingleColumn>()
        })
        .await
        .expect("project_all columns");

    rows.sort_by(|a, b| a.category.cmp(&b.category));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].category, "books");
    assert_eq!(rows[1].category, "electronics");
}

// ════════════════════════════════════════════════════════════════════
// Tests: JOIN + projection (shared column names like tenant_id)
// ════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn project_all_with_join_disambiguates_tenant_id() {
    let db = TestDb::new().await;
    let conn = db.conn();
    let tid = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tid);

    let o1 = insert_order(&conn, &scope, tid, "books", 10).await;
    let o2 = insert_order(&conn, &scope, tid, "electronics", 50).await;

    // o1 has 3 items, o2 has 1 item
    insert_item(&conn, &scope, tid, o1, 2).await;
    insert_item(&conn, &scope, tid, o1, 3).await;
    insert_item(&conn, &scope, tid, o1, 1).await;
    insert_item(&conn, &scope, tid, o2, 5).await;

    // JOIN orders ← items, GROUP BY order_id, COUNT(*)
    // Both tables have tenant_id — this tests that scope_with generates
    // table-qualified SQL even through a join.
    let mut rows: Vec<OrderItemCount> = order_ent::Entity::find()
        .join(JoinType::InnerJoin, order_ent::Relation::Items.def())
        .secure()
        .scope_with(&scope)
        .project_all(&conn, |q| {
            q.select_only()
                .column_as(order_ent::Column::Id, "order_id")
                .column_as(item_ent::Column::Id.count(), "cnt")
                .group_by(order_ent::Column::Id)
                .into_model::<OrderItemCount>()
        })
        .await
        .expect("project_all join");

    rows.sort_by_key(|r| r.cnt);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].order_id, o2);
    assert_eq!(rows[0].cnt, 1);
    assert_eq!(rows[1].order_id, o1);
    assert_eq!(rows[1].cnt, 3);
}

#[tokio::test]
async fn project_all_join_cross_tenant_isolation() {
    let db = TestDb::new().await;
    let conn = db.conn();
    let tid_a = Uuid::new_v4();
    let tid_b = Uuid::new_v4();
    let scope_a = AccessScope::for_tenant(tid_a);
    let scope_b = AccessScope::for_tenant(tid_b);

    let o1 = insert_order(&conn, &scope_a, tid_a, "books", 10).await;
    insert_item(&conn, &scope_a, tid_a, o1, 2).await;

    // Query with tenant B scope — must return empty even with a join
    let rows: Vec<OrderItemCount> = order_ent::Entity::find()
        .join(JoinType::InnerJoin, order_ent::Relation::Items.def())
        .secure()
        .scope_with(&scope_b)
        .project_all(&conn, |q| {
            q.select_only()
                .column_as(order_ent::Column::Id, "order_id")
                .column_as(item_ent::Column::Id.count(), "cnt")
                .group_by(order_ent::Column::Id)
                .into_model::<OrderItemCount>()
        })
        .await
        .expect("project_all join cross-tenant");

    assert!(rows.is_empty(), "cross-tenant join must return empty");
}

// ════════════════════════════════════════════════════════════════════
// Tests: filter + projection combined
// ════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn project_all_with_pre_filter() {
    let db = TestDb::new().await;
    let conn = db.conn();
    let tid = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tid);

    insert_order(&conn, &scope, tid, "books", 10).await;
    insert_order(&conn, &scope, tid, "books", 20).await;
    insert_order(&conn, &scope, tid, "electronics", 50).await;

    // Pre-filter to only "books" before projection
    let rows: Vec<CategoryTotal> = order_ent::Entity::find()
        .filter(order_ent::Column::Category.eq("books"))
        .secure()
        .scope_with(&scope)
        .project_all(&conn, |q| {
            q.select_only()
                .column(order_ent::Column::Category)
                .column_as(Expr::col(order_ent::Column::Amount).sum(), "total")
                .group_by(order_ent::Column::Category)
                .into_model::<CategoryTotal>()
        })
        .await
        .expect("project_all with filter");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].category, "books");
    assert_eq!(rows[0].total, 30);
}
