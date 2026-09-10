#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(all(
    feature = "integration",
    any(feature = "sqlite", feature = "pg", feature = "mysql")
))]

mod common;
use anyhow::Result;
use sea_orm::EntityTrait;
use sea_orm_migration::prelude as mig;
use sea_orm_migration::prelude::Iden;
use sea_orm_migration::sea_query;
use toolkit_db::secure::SecureEntityExt;
use toolkit_security::pep_properties;

// Create a simple table via migrations (runtime-privileged path).
#[derive(Iden)]
enum GenericTbl {
    #[iden = "test_generic"]
    Table,
    Id,
    TenantId,
    Name,
}

struct CreateGeneric;
impl mig::MigrationName for CreateGeneric {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "m001_create_test_generic"
    }
}
#[async_trait::async_trait]
impl mig::MigrationTrait for CreateGeneric {
    async fn up(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .create_table(
                mig::Table::create()
                    .table(GenericTbl::Table)
                    .if_not_exists()
                    .col(
                        mig::ColumnDef::new(GenericTbl::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(mig::ColumnDef::new(GenericTbl::TenantId).uuid().not_null())
                    .col(mig::ColumnDef::new(GenericTbl::Name).string().not_null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .drop_table(mig::Table::drop().table(GenericTbl::Table).to_owned())
            .await
    }
}

// Define a minimal entity mapping for secure query execution.
mod ent {
    use sea_orm::entity::prelude::*;
    use uuid::Uuid;

    #[derive(Debug, Clone, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "test_generic")]
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

impl toolkit_db::secure::ScopableEntity for ent::Entity {
    fn tenant_col() -> Option<<Self as sea_orm::EntityTrait>::Column> {
        Some(ent::Column::TenantId)
    }
    fn resource_col() -> Option<<Self as sea_orm::EntityTrait>::Column> {
        Some(ent::Column::Id)
    }
    fn owner_col() -> Option<<Self as sea_orm::EntityTrait>::Column> {
        None
    }
    fn type_col() -> Option<<Self as sea_orm::EntityTrait>::Column> {
        None
    }
    fn resolve_property(property: &str) -> Option<<Self as sea_orm::EntityTrait>::Column> {
        match property {
            p if p == pep_properties::OWNER_TENANT_ID => Self::tenant_col(),
            p if p == pep_properties::RESOURCE_ID => Self::resource_col(),
            _ => None,
        }
    }
    fn scope_columns() -> Vec<<Self as sea_orm::EntityTrait>::Column> {
        vec![ent::Column::TenantId, ent::Column::Id]
    }
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn generic_sqlite() -> Result<()> {
    let dut = common::bring_up_sqlite();
    run_common_suite(&dut.url).await
}

#[cfg(feature = "pg")]
#[tokio::test]
async fn generic_postgres() -> Result<()> {
    let dut = common::bring_up_postgres().await?;
    run_common_suite(&dut.url).await
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn generic_mysql() -> Result<()> {
    let dut = common::bring_up_mysql().await?;
    run_common_suite(&dut.url).await
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn outbox_schema_reapplies_cleanly_sqlite() -> Result<()> {
    let dut = common::bring_up_sqlite();
    run_outbox_reapply_suite(&dut.url).await
}

#[cfg(feature = "pg")]
#[tokio::test]
async fn outbox_schema_reapplies_cleanly_postgres() -> Result<()> {
    let dut = common::bring_up_postgres().await?;
    run_outbox_reapply_suite(&dut.url).await
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn outbox_schema_reapplies_cleanly_mysql() -> Result<()> {
    let dut = common::bring_up_mysql().await?;
    run_outbox_reapply_suite(&dut.url).await
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn cte_shapes_are_valid_sql_sqlite() -> Result<()> {
    let dut = common::bring_up_sqlite();
    run_cte_suite(&dut.url).await
}

#[cfg(feature = "pg")]
#[tokio::test]
async fn cte_shapes_are_valid_sql_postgres() -> Result<()> {
    let dut = common::bring_up_postgres().await?;
    run_cte_suite(&dut.url).await
}

#[cfg(feature = "mysql")]
#[tokio::test]
async fn cte_shapes_are_valid_sql_mysql() -> Result<()> {
    let dut = common::bring_up_mysql().await?;
    run_cte_suite(&dut.url).await
}

/// Applies the identical outbox DDL twice over the same database.
///
/// This is the crash-before-recording-then-retry path: every `CREATE TABLE` and
/// `CREATE INDEX` in the outbox schema meets an object that already exists. Two
/// *gear* names are used because each gear tracks its migration history
/// independently, so the second run genuinely re-executes the DDL instead of
/// skipping it -- which the `applied` assertions below pin down.
///
/// Covers the backend that the unit tests can only assert at the SQL-string
/// level: on `MySQL` there is no `CREATE INDEX IF NOT EXISTS`, so idempotency
/// rests on the `information_schema` probe actually working against a live
/// server.
async fn run_outbox_reapply_suite(database_url: &str) -> Result<()> {
    let db = toolkit_db::connect_db(database_url, toolkit_db::ConnectOpts::default()).await?;

    let first = toolkit_db::migration_runner::run_migrations_for_gear(
        &db,
        "outbox_reapply_first",
        toolkit_db::outbox::outbox_migrations(),
    )
    .await
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    assert_eq!(first.applied, 1, "first run should create the schema");

    // Same DDL, fresh migration history -> re-executed against the schema the
    // first run just built. Fails on the very first index without idempotent
    // index creation.
    let second = toolkit_db::migration_runner::run_migrations_for_gear(
        &db,
        "outbox_reapply_second",
        toolkit_db::outbox::outbox_migrations(),
    )
    .await
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    assert_eq!(
        second.applied, 1,
        "second run must actually re-execute the DDL, not skip it"
    );
    assert_eq!(second.skipped, 0);

    Ok(())
}

/// Runs the same assertions for any backend.
async fn run_common_suite(database_url: &str) -> Result<()> {
    // Test basic connection
    let db = toolkit_db::connect_db(database_url, toolkit_db::ConnectOpts::default()).await?;

    // Test DSN redaction (should not panic)
    let redacted = toolkit_db::redact_credentials_in_dsn(Some(database_url));
    assert!(!redacted.contains("pass"));

    // Test basic transaction + ORM ops using the secure API (no raw SQLx/SeaORM connections).
    toolkit_db::migration_runner::run_migrations_for_testing(&db, vec![Box::new(CreateGeneric)])
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    // Use the secure Db API
    let secure_db = db;

    let tenant_id = uuid::Uuid::new_v4();
    let scope = toolkit_security::AccessScope::for_tenants(vec![tenant_id]);
    let scope_for_tx = scope.clone();
    let id = uuid::Uuid::new_v4();

    let (secure_db, res) = secure_db
        .transaction(move |tx| {
            let scope = scope_for_tx.clone();
            Box::pin(async move {
                let am = ent::ActiveModel {
                    id: sea_orm::Set(id),
                    tenant_id: sea_orm::Set(tenant_id),
                    name: sea_orm::Set("test_user".to_owned()),
                };
                let _ = toolkit_db::secure::secure_insert::<ent::Entity>(am, &scope, tx).await?;
                Ok::<(), anyhow::Error>(())
            })
        })
        .await;
    res?;

    let conn = secure_db
        .conn()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let count = ent::Entity::find()
        .secure()
        .scope_with(&scope)
        .count(&conn)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    assert_eq!(count, 1);

    Ok(())
}

// ════════════════════════════════════════════════════════════════════
// CTE suite -- runs on every backend
// ════════════════════════════════════════════════════════════════════
//
// The unit tests in `src/secure/cte_tests.rs` assert on rendered SQL *text*, so
// they cannot tell whether a backend actually accepts the statement. That gap let
// a real defect through: `distinct()` combined with ordering by a CTE column is
// rejected by PostgreSQL and MySQL but accepted by SQLite, so a SQLite-only
// integration suite reported success on SQL that fails in production.
//
// This suite therefore executes every query *shape* the CTE builder can emit,
// once per backend. Assertions stay shallow on purpose -- the claim under test is
// "the server accepts this", not "the rows are right". Row-level semantics
// (tenant isolation, subtree membership, depth truncation, cycle termination) live
// in `tests/sqlite/secure_cte.rs`, where in-memory SQLite makes them cheap.

/// Separate table from `test_generic`: `recursive_cte` needs a self-referencing
/// column, and `ent` is shared with the suites above.
#[derive(Iden)]
enum CteTbl {
    #[iden = "test_cte_node"]
    Table,
    Id,
    TenantId,
    ParentId,
    Name,
    Payload,
}

struct CreateCteNode;
impl mig::MigrationName for CreateCteNode {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "m001_create_test_cte_node"
    }
}

#[async_trait::async_trait]
impl mig::MigrationTrait for CreateCteNode {
    async fn up(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .create_table(
                sea_query::Table::create()
                    .table(CteTbl::Table)
                    .if_not_exists()
                    .col(
                        mig::ColumnDef::new(CteTbl::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(mig::ColumnDef::new(CteTbl::TenantId).uuid().not_null())
                    .col(mig::ColumnDef::new(CteTbl::ParentId).uuid())
                    .col(mig::ColumnDef::new(CteTbl::Name).string().not_null())
                    // Deliberately `json`, not `jsonb`: on PostgreSQL `json` has no
                    // equality operator, so selecting it inside a UNION-deduplicated
                    // recursive CTE fails with "could not identify an equality
                    // operator for type json". The recursive body must therefore
                    // project only the traversal columns. This column exists to keep
                    // that regression caught.
                    .col(mig::ColumnDef::new(CteTbl::Payload).json())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .drop_table(sea_query::Table::drop().table(CteTbl::Table).to_owned())
            .await
    }
}

mod cte_ent {
    use sea_orm::entity::prelude::*;

    #[derive(Debug, Clone, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "test_cte_node")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub parent_id: Option<Uuid>,
        pub name: String,
        #[sea_orm(column_type = "Json", nullable)]
        pub payload: Option<serde_json::Value>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

impl toolkit_db::secure::ScopableEntity for cte_ent::Entity {
    fn tenant_col() -> Option<<Self as EntityTrait>::Column> {
        Some(cte_ent::Column::TenantId)
    }
    fn resource_col() -> Option<<Self as EntityTrait>::Column> {
        Some(cte_ent::Column::Id)
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
        vec![cte_ent::Column::TenantId, cte_ent::Column::Id]
    }
}

/// Narrow projection: the point of `all_as` is to avoid materialising the full
/// model, so the cross-backend check uses a type that is not `E::Model`.
#[derive(Debug, sea_orm::FromQueryResult)]
struct NodeName {
    name: String,
}

/// Row type for the `DISTINCT` shapes. They must not select the `json` column:
/// `SELECT DISTINCT` compares every selected column and `PostgreSQL`'s `json` has no
/// equality operator, so distinct-over-the-whole-entity is not portable. Narrowing
/// the projection is the fix, which is what these shapes demonstrate.
#[derive(Debug, sea_orm::FromQueryResult)]
struct NodeIdName {
    name: String,
}

async fn run_cte_suite(database_url: &str) -> Result<()> {
    use sea_orm::sea_query::{Alias, Expr, Order};
    use sea_orm::{ColumnTrait, Condition, ExprTrait, Set};
    use toolkit_db::secure::RecursiveCte;

    let db = toolkit_db::connect_db(database_url, toolkit_db::ConnectOpts::default()).await?;
    toolkit_db::migration_runner::run_migrations_for_testing(&db, vec![Box::new(CreateCteNode)])
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let tenant_id = uuid::Uuid::new_v4();
    let scope = toolkit_security::AccessScope::for_tenants(vec![tenant_id]);
    let conn = db.conn().map_err(|e| anyhow::anyhow!(e.to_string()))?;

    // root -> child, so the recursive walk has at least one real hop to take.
    let root = uuid::Uuid::new_v4();
    let child = uuid::Uuid::new_v4();
    for (id, parent, name) in [(root, None, "root"), (child, Some(root), "child")] {
        let am = cte_ent::ActiveModel {
            id: Set(id),
            tenant_id: Set(tenant_id),
            parent_id: Set(parent),
            name: Set(name.to_owned()),
            payload: Set(Some(serde_json::json!({ "n": name }))),
        };
        toolkit_db::secure::secure_insert::<cte_ent::Entity>(am, &scope, &conn)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    }

    let scoped = || cte_ent::Entity::find().secure().scope_with(&scope);
    let join_on_id = |cte: &'static str| {
        Condition::all().add(
            Expr::col((Alias::new(cte), Alias::new("id")))
                .equals((cte_ent::Entity, cte_ent::Column::Id)),
        )
    };
    let subtree = || {
        RecursiveCte::<cte_ent::Entity>::new(
            "subtree",
            Condition::all().add(cte_ent::Column::Id.eq(root)),
            cte_ent::Column::ParentId,
            cte_ent::Column::Id,
            5,
        )
    };

    // Shape 1: flat CTE, joined back to the outer entity.
    let flat = scoped()
        .with_ctes()
        .cte::<cte_ent::Entity>("all_nodes", |q| q)
        .join_cte("all_nodes", join_on_id("all_nodes"))
        .all(&conn)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    assert_eq!(flat.len(), 2, "flat CTE should return both seeded rows");

    // Shape 2: recursive CTE. Two rows means the recursive member ran -- a seed
    // that never recursed would return only the root.
    let walked = scoped()
        .with_ctes()
        .recursive_cte(subtree())
        .join_cte("subtree", join_on_id("subtree"))
        .all(&conn)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    assert_eq!(walked.len(), 2, "recursive walk should reach the child");

    // Shape 3: DISTINCT over a narrowed projection.
    //
    // The narrowing is not incidental. This entity carries a `json` column, and
    // `SELECT DISTINCT` compares every selected column -- PostgreSQL's `json` has
    // no equality operator, so distinct over the full entity is not portable.
    // `select_only()` is what makes DISTINCT usable on such an entity.
    let deduped = scoped()
        .with_ctes()
        .cte::<cte_ent::Entity>("all_nodes", |q| q)
        .join_cte("all_nodes", join_on_id("all_nodes"))
        .select_only()
        .column(cte_ent::Column::Name)
        .distinct()
        .all_as::<NodeIdName>(&conn)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    assert_eq!(deduped.len(), 2);

    // Shape 4: DISTINCT + ORDER BY an entity column. Valid everywhere, because an
    // entity column is in the outer SELECT list.
    let ordered = scoped()
        .with_ctes()
        .cte::<cte_ent::Entity>("all_nodes", |q| q)
        .join_cte("all_nodes", join_on_id("all_nodes"))
        .select_only()
        .column(cte_ent::Column::Name)
        .distinct()
        .order_by(cte_ent::Column::Name, Order::Asc)
        .all_as::<NodeIdName>(&conn)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    assert_eq!(
        ordered.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        vec!["child", "root"],
        "ORDER BY on an entity column should sort"
    );

    // Shape 5: ORDER BY a CTE column, without DISTINCT. Valid on all three.
    let by_depth = scoped()
        .with_ctes()
        .recursive_cte(subtree())
        .join_cte("subtree", join_on_id("subtree"))
        .order_by_cte(
            "subtree",
            RecursiveCte::<cte_ent::Entity>::DEPTH_COLUMN,
            Order::Asc,
        )
        .all(&conn)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    assert_eq!(
        by_depth.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        vec!["root", "child"],
        "ordering by depth should put the seed first"
    );

    // Shape 6: narrow projection through all_as.
    let mut names = scoped()
        .with_ctes()
        .cte::<cte_ent::Entity>("all_nodes", |q| q)
        .join_cte("all_nodes", join_on_id("all_nodes"))
        .all_as::<NodeName>(&conn)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .into_iter()
        .map(|n| n.name)
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, vec!["child", "root"]);

    // Shape 7: the rejected pairing. This is the regression guard -- PostgreSQL
    // and MySQL reject this SQL outright, SQLite accepts it, so the library must
    // refuse it identically on every backend BEFORE reaching the server.
    let conflict = scoped()
        .with_ctes()
        .recursive_cte(subtree())
        .join_cte("subtree", join_on_id("subtree"))
        .select_only()
        .column(cte_ent::Column::Name)
        .distinct()
        .order_by_cte(
            "subtree",
            RecursiveCte::<cte_ent::Entity>::DEPTH_COLUMN,
            Order::Asc,
        )
        .all(&conn)
        .await;
    let Err(err) = conflict else {
        anyhow::bail!("distinct() + order_by_cte() must be rejected, not executed");
    };
    assert!(
        matches!(err, toolkit_db::secure::ScopeError::Invalid(msg) if msg.contains("DISTINCT")),
        "expected a typed rejection, got: {err:?}"
    );

    Ok(())
}
