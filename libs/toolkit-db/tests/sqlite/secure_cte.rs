#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for CTE support in the Secure ORM (ADR-0001).
//!
//! The unit tests in `src/secure/cte_tests.rs` assert what SQL is *built*. These
//! assert what it *returns*, which is the only way to show the isolation claim
//! actually holds and that a recursive walk terminates.
//!
//! Security contract:
//! - No raw SQL in tests.
//! - Schema is created via `sea-orm-migration` definitions run by the migration runner.

use sea_orm::entity::prelude::*;
use sea_orm::sea_query::{Alias, Expr};
use sea_orm::{Condition, ExprTrait, Set};
use sea_orm_migration::prelude as mig;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{
    Db, DbConn, RecursiveCte, ScopableEntity, SecureEntityExt, SecureUpdateExt, secure_insert,
};
use toolkit_db::{ConnectOpts, connect_db};
use toolkit_security::{AccessScope, pep_properties};
use uuid::Uuid;

// ════════════════════════════════════════════════════════════════════
// Test entity: a tenant-scoped tree
// ════════════════════════════════════════════════════════════════════

mod node {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "cte_nodes")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub parent_id: Option<Uuid>,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

impl ScopableEntity for node::Entity {
    fn tenant_col() -> Option<<Self as EntityTrait>::Column> {
        Some(node::Column::TenantId)
    }
    fn resource_col() -> Option<<Self as EntityTrait>::Column> {
        Some(node::Column::Id)
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
        vec![node::Column::TenantId, node::Column::Id]
    }
}

// ════════════════════════════════════════════════════════════════════
// Migration
// ════════════════════════════════════════════════════════════════════

struct CreateCteTables;

impl mig::MigrationName for CreateCteTables {
    fn name(&self) -> &'static str {
        "m001_create_cte_tables"
    }
}

#[async_trait::async_trait]
impl mig::MigrationTrait for CreateCteTables {
    async fn up(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .create_table(
                mig::Table::create()
                    .table(mig::Alias::new("cte_nodes"))
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
                    .col(mig::ColumnDef::new(mig::Alias::new("parent_id")).uuid())
                    .col(
                        mig::ColumnDef::new(mig::Alias::new("name"))
                            .string()
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
                    .table(mig::Alias::new("cte_nodes"))
                    .to_owned(),
            )
            .await
    }
}

// ════════════════════════════════════════════════════════════════════
// Harness
// ════════════════════════════════════════════════════════════════════

struct TestDb {
    db: Db,
}

impl TestDb {
    async fn new(name: &str) -> Self {
        let test_id = Uuid::new_v4();
        let dsn = format!("sqlite:file:memdb_cte_{name}_{test_id}?mode=memory&cache=shared");
        let opts = ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        };
        let db = connect_db(&dsn, opts).await.expect("db connect");
        run_migrations_for_testing(&db, vec![Box::new(CreateCteTables)])
            .await
            .expect("migrate");
        Self { db }
    }

    fn conn(&self) -> DbConn<'_> {
        self.db.conn().expect("conn")
    }
}

/// Insert one node. Uses `secure_insert` so the row is written through the same
/// scope machinery the reads go through.
async fn insert_node(
    conn: &DbConn<'_>,
    scope: &AccessScope,
    id: Uuid,
    tenant: Uuid,
    parent: Option<Uuid>,
    name: &str,
) {
    let am = node::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant),
        parent_id: Set(parent),
        name: Set(name.to_owned()),
    };
    secure_insert::<node::Entity>(am, scope, conn)
        .await
        .expect("insert node");
}

/// `root -> child -> grandchild` plus a detached sibling, all in one tenant.
struct Tree {
    root: Uuid,
    child: Uuid,
    grandchild: Uuid,
    detached: Uuid,
}

async fn seed_tree(conn: &DbConn<'_>, scope: &AccessScope, tenant: Uuid, prefix: &str) -> Tree {
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    let grandchild = Uuid::new_v4();
    let detached = Uuid::new_v4();

    insert_node(conn, scope, root, tenant, None, &format!("{prefix}-root")).await;
    insert_node(
        conn,
        scope,
        child,
        tenant,
        Some(root),
        &format!("{prefix}-child"),
    )
    .await;
    insert_node(
        conn,
        scope,
        grandchild,
        tenant,
        Some(child),
        &format!("{prefix}-grandchild"),
    )
    .await;
    insert_node(
        conn,
        scope,
        detached,
        tenant,
        None,
        &format!("{prefix}-detached"),
    )
    .await;

    Tree {
        root,
        child,
        grandchild,
        detached,
    }
}

/// `ON cte.id = cte_nodes.id`, i.e. narrow the outer entity to the CTE's rows.
fn join_on_id(cte: &'static str) -> Condition {
    Condition::all().add(
        Expr::col((Alias::new(cte), Alias::new("id"))).equals((node::Entity, node::Column::Id)),
    )
}

fn subtree_of(root: Uuid, max_depth: u32) -> RecursiveCte<node::Entity> {
    RecursiveCte::new(
        "subtree",
        Condition::all().add(node::Column::Id.eq(root)),
        node::Column::ParentId,
        node::Column::Id,
        max_depth,
    )
}

// ════════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn cte_query_returns_only_rows_of_the_scoped_tenant() {
    let tdb = TestDb::new("isolation").await;
    let conn = tdb.conn();

    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let scope_a = AccessScope::for_tenants(vec![tenant_a]);
    let scope_b = AccessScope::for_tenants(vec![tenant_b]);

    let a = seed_tree(&conn, &scope_a, tenant_a, "a").await;
    seed_tree(&conn, &scope_b, tenant_b, "b").await;

    // What the CTE body itself contains. Joining on `id` would re-filter through
    // the outer query's own tenant predicate, which masks an unscoped body
    // completely -- so that shape cannot detect the leak this test exists for.
    // Pinning the outer query to one row and joining on a tautology gives one
    // result row per CTE entry, making the body's size observable.
    let cte_entries = node::Entity::find()
        .secure()
        .scope_with(&scope_a)
        .with_ctes()
        .cte::<node::Entity>("all_nodes", |q| q)
        .filter(Condition::all().add(node::Column::Id.eq(a.root)))
        .join_cte(
            "all_nodes",
            Condition::all().add(Expr::val(1).eq(Expr::val(1))),
        )
        .all(&conn)
        .await
        .expect("cte query")
        .len();

    assert_eq!(
        cte_entries, 4,
        "the CTE body should hold only tenant A's four nodes; 8 means the body \
         was not scoped and tenant B's rows are inside the WITH"
    );

    // And the ordinary shape returns tenant A's rows.
    let rows = node::Entity::find()
        .secure()
        .scope_with(&scope_a)
        .with_ctes()
        .cte::<node::Entity>("all_nodes", |q| q)
        .join_cte("all_nodes", join_on_id("all_nodes"))
        .all(&conn)
        .await
        .expect("cte query");

    assert_eq!(rows.len(), 4, "tenant A seeded four nodes");
    assert!(
        rows.iter().all(|r| r.tenant_id == tenant_a),
        "leaked rows from another tenant: {rows:?}"
    );
    assert!(rows.iter().any(|r| r.id == a.root));
}

#[tokio::test]
async fn recursive_cte_walks_the_subtree_and_excludes_unrelated_rows() {
    let tdb = TestDb::new("subtree").await;
    let conn = tdb.conn();

    let tenant = Uuid::new_v4();
    let scope = AccessScope::for_tenants(vec![tenant]);
    let t = seed_tree(&conn, &scope, tenant, "t").await;

    let rows = node::Entity::find()
        .secure()
        .scope_with(&scope)
        .with_ctes()
        .recursive_cte(subtree_of(t.root, 10))
        .join_cte("subtree", join_on_id("subtree"))
        .all(&conn)
        .await
        .expect("recursive cte query");

    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    assert!(ids.contains(&t.root), "seed row missing: {ids:?}");
    assert!(ids.contains(&t.child), "depth-1 row missing: {ids:?}");
    assert!(ids.contains(&t.grandchild), "depth-2 row missing: {ids:?}");
    assert!(
        !ids.contains(&t.detached),
        "a node outside the subtree was returned: {ids:?}"
    );
    assert_eq!(ids.len(), 3);
}

#[tokio::test]
async fn recursive_cte_never_crosses_a_tenant_boundary() {
    // The reason the recursive member carries scope too. Both tenants get an
    // identically shaped tree, so a walk that ignored scope after the seed would
    // still find rows -- just the wrong ones.
    let tdb = TestDb::new("cross_tenant").await;
    let conn = tdb.conn();

    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let scope_a = AccessScope::for_tenants(vec![tenant_a]);
    let scope_b = AccessScope::for_tenants(vec![tenant_b]);

    let a = seed_tree(&conn, &scope_a, tenant_a, "a").await;
    let b = seed_tree(&conn, &scope_b, tenant_b, "b").await;

    // Deliberately graft tenant B's root under tenant A's child. The edge exists
    // in the data, so only the scope predicate in the recursive member stops the
    // walk from following it.
    node::Entity::update_many()
        .secure()
        .scope_with(&scope_b)
        .col_expr(node::Column::ParentId, Expr::value(a.child))
        .filter(Condition::all().add(node::Column::Id.eq(b.root)))
        .exec(&conn)
        .await
        .expect("graft b under a");

    // Observing this needs care. Joining the CTE back on `id` would re-filter
    // through the *outer* query's tenant predicate, which hides a leaking CTE
    // body entirely -- the outer WHERE drops the foreign rows before they are
    // visible, so the test would pass even with an unscoped recursive member.
    //
    // Instead: pin the outer query to a single row and join the CTE on a
    // tautology, so the result has exactly one row per CTE entry. Then the row
    // *count* reports how far the walk actually went, leak included.
    let cte_entries = node::Entity::find()
        .secure()
        .scope_with(&scope_a)
        .with_ctes()
        .recursive_cte(subtree_of(a.root, 10))
        .filter(Condition::all().add(node::Column::Id.eq(a.root)))
        .join_cte(
            "subtree",
            Condition::all().add(Expr::val(1).eq(Expr::val(1))),
        )
        .all(&conn)
        .await
        .expect("recursive cte query")
        .len();

    assert_eq!(
        cte_entries, 3,
        "the walk should visit exactly tenant A's three nodes; more means it \
         followed the grafted edge into tenant B"
    );

    // And the ordinary shape still returns only tenant A's rows.
    let rows = node::Entity::find()
        .secure()
        .scope_with(&scope_a)
        .with_ctes()
        .recursive_cte(subtree_of(a.root, 10))
        .join_cte("subtree", join_on_id("subtree"))
        .all(&conn)
        .await
        .expect("recursive cte query");

    assert!(
        rows.iter().all(|r| r.tenant_id == tenant_a),
        "walk crossed into another tenant: {rows:?}"
    );
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    assert!(
        !ids.contains(&b.root) && !ids.contains(&b.child),
        "tenant B rows reachable through the grafted edge: {ids:?}"
    );
    assert_eq!(ids.len(), 3, "still exactly tenant A's subtree");
}

#[tokio::test]
async fn recursive_cte_depth_cap_truncates_the_walk() {
    let tdb = TestDb::new("depth").await;
    let conn = tdb.conn();

    let tenant = Uuid::new_v4();
    let scope = AccessScope::for_tenants(vec![tenant]);
    let t = seed_tree(&conn, &scope, tenant, "t").await;

    let ids_at = |depth: u32| {
        let scope = scope.clone();
        let conn = &conn;
        async move {
            let rows = node::Entity::find()
                .secure()
                .scope_with(&scope)
                .with_ctes()
                .recursive_cte(subtree_of(t.root, depth))
                .join_cte("subtree", join_on_id("subtree"))
                .all(conn)
                .await
                .expect("recursive cte query");
            rows.iter().map(|r| r.id).collect::<Vec<Uuid>>()
        }
    };

    let depth0 = ids_at(0).await;
    assert_eq!(depth0, vec![t.root], "depth 0 is the seed row only");

    let depth1 = ids_at(1).await;
    assert_eq!(depth1.len(), 2, "depth 1 adds direct children: {depth1:?}");
    assert!(depth1.contains(&t.child));
    assert!(
        !depth1.contains(&t.grandchild),
        "depth 1 must not reach depth 2: {depth1:?}"
    );

    assert_eq!(ids_at(2).await.len(), 3, "depth 2 reaches the grandchild");
}

#[tokio::test]
async fn recursive_cte_terminates_on_a_cyclic_graph() {
    // sea_query drops PostgreSQL's CYCLE clause on MySQL/SQLite, so the depth cap
    // is the only portable termination guarantee. Without it this query would
    // recurse until the server gave up.
    let tdb = TestDb::new("cycle").await;
    let conn = tdb.conn();

    let tenant = Uuid::new_v4();
    let scope = AccessScope::for_tenants(vec![tenant]);
    let t = seed_tree(&conn, &scope, tenant, "t").await;

    // Close the loop: grandchild becomes the parent of root.
    node::Entity::update_many()
        .secure()
        .scope_with(&scope)
        .col_expr(node::Column::ParentId, Expr::value(t.grandchild))
        .filter(Condition::all().add(node::Column::Id.eq(t.root)))
        .exec(&conn)
        .await
        .expect("create cycle");

    let walk = |cap: u32| {
        let scope = scope.clone();
        let conn = &conn;
        async move {
            node::Entity::find()
                .secure()
                .scope_with(&scope)
                .with_ctes()
                .recursive_cte(subtree_of(t.root, cap))
                .join_cte("subtree", join_on_id("subtree"))
                .all(conn)
                .await
                .expect("cyclic recursive cte must terminate, not hang or error")
        }
    };

    // Going round the loop revisits nodes, so rows *repeat*: the walk yields one
    // row per hop, not per distinct node, and nothing deduplicates it. The depth
    // cap is what makes that finite -- row count tracks the cap exactly, which is
    // the property being asserted. (Callers who want distinct nodes must dedup
    // themselves; see `RecursiveCte`'s docs.)
    let capped_3 = walk(3).await;
    let capped_5 = walk(5).await;
    assert_eq!(capped_3.len(), 4, "cap 3 should yield hops 0..=3");
    assert_eq!(capped_5.len(), 6, "cap 5 should yield hops 0..=5");

    // And it is genuinely cycling over the three real nodes, not inventing rows.
    let mut distinct: Vec<Uuid> = capped_5.iter().map(|r| r.id).collect();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        3,
        "expected the 3 cycle members, got {distinct:?}"
    );
}

#[tokio::test]
async fn cte_query_deserializes_into_a_custom_projection() {
    // CTEs usually exist to compute something the entity model has no shape for,
    // so `all_as` is the common case rather than an afterthought.
    #[derive(Debug, sea_orm::FromQueryResult)]
    struct NodeName {
        name: String,
    }

    let tdb = TestDb::new("projection").await;
    let conn = tdb.conn();

    let tenant = Uuid::new_v4();
    let scope = AccessScope::for_tenants(vec![tenant]);
    seed_tree(&conn, &scope, tenant, "p").await;

    let names = node::Entity::find()
        .secure()
        .scope_with(&scope)
        .with_ctes()
        .cte::<node::Entity>("all_nodes", |q| q)
        .join_cte("all_nodes", join_on_id("all_nodes"))
        .all_as::<NodeName>(&conn)
        .await
        .expect("projection query");

    let mut got: Vec<String> = names.into_iter().map(|n| n.name).collect();
    got.sort();
    assert_eq!(
        got,
        vec!["p-child", "p-detached", "p-grandchild", "p-root"],
        "projection lost or duplicated rows"
    );
}

#[tokio::test]
async fn distinct_collapses_rows_duplicated_by_the_cte_join() {
    // `join_cte` is an inner join, so a node with two matching CTE rows comes back
    // twice. This is the frontier-expansion case: several edges converge on one
    // node. Without `distinct()` a BFS hop would double-count.
    let tdb = TestDb::new("distinct").await;
    let conn = tdb.conn();

    let tenant = Uuid::new_v4();
    let scope = AccessScope::for_tenants(vec![tenant]);
    let t = seed_tree(&conn, &scope, tenant, "d").await;

    // Two extra children of root, so `parent_id = root` matches three rows.
    for name in ["extra-1", "extra-2"] {
        insert_node(&conn, &scope, Uuid::new_v4(), tenant, Some(t.root), name).await;
    }

    // CTE = every node whose parent is root; joined to the outer entity on the
    // shared parent, so root itself matches once per CTE row.
    let children_of_root = |distinct: bool| {
        let scope = scope.clone();
        let conn = &conn;
        async move {
            let q = node::Entity::find()
                .secure()
                .scope_with(&scope)
                .with_ctes()
                .cte::<node::Entity>("kids", |q| {
                    q.filter(Condition::all().add(node::Column::ParentId.eq(t.root)))
                })
                .filter(Condition::all().add(node::Column::Id.eq(t.root)))
                .join_cte(
                    "kids",
                    Condition::all().add(
                        Expr::col((Alias::new("kids"), Alias::new("parent_id")))
                            .equals((node::Entity, node::Column::Id)),
                    ),
                );
            let q = if distinct { q.distinct() } else { q };
            q.all(conn).await.expect("cte query")
        }
    };

    let duplicated = children_of_root(false).await;
    assert_eq!(
        duplicated.len(),
        3,
        "inner join should repeat the outer row once per CTE match"
    );

    let collapsed = children_of_root(true).await;
    assert_eq!(collapsed.len(), 1, "distinct() should collapse them to one");
    assert_eq!(collapsed[0].id, t.root);
}
