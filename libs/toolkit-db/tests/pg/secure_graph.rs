#![allow(clippy::unwrap_used, clippy::expect_used)]

//! What a secure graph query *returns* on a real `PostgreSQL` 19 server.
//!
//! The unit tests assert what SQL is built. These assert what comes back, which
//! is the only way to show the isolation claim holds rather than merely renders.
//!
//! # Two fixtures, two failure modes
//!
//! **The composite-key graph** (`pgq_kb`) keys every element on
//! `(tenant_id, id)`, as every graph this platform declares does. Both tenants
//! own a node `11`, so a traversal that leaks across tenants manifests as an
//! *extra row* with a familiar id — which is why every assertion here is on the
//! raw, sorted, **non-deduplicated** id list: `dedup()` would discard exactly
//! the signal these tests exist to observe.
//!
//! **The open graph** (`pgq_open`) keys elements on `id` alone, so an edge can
//! resolve endpoints across tenant boundaries. That is the fixture where a
//! single unscoped element body demonstrably returns foreign rows — on the
//! composite-key graph the endpoint key blocks cross-tenant joins structurally,
//! so an edge-scope hole is invisible there.
//!
//! # Observing the hole
//!
//! A test that only checks "the scoped query returns the right rows" cannot
//! tell a working guard from an absent one. So the suite also runs *control*
//! queries built straight from the syntax crate (which embeds no scope) over a
//! plain connection, and asserts they do return what the guards exist to hide.
//! If a control ever stops seeing the foreign rows, the guard tests are no
//! longer evidence of anything — which is also why the fixture rows the
//! controls rely on are asserted as preconditions first.

use sea_orm::entity::prelude::*;
use sea_orm::sea_query::ExprTrait as _;
use sea_orm::{FromQueryResult as _, Set};
use sea_orm_migration::prelude as mig;
use testcontainers::runners::AsyncRunner as _;
use testcontainers::{ContainerAsync, ImageExt as _};
use testcontainers_modules::postgres::Postgres;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::pgq::{Endpoint, GraphDeclaration, PropertyGraph, VertexOf};
use toolkit_db::secure::{Db, DbConn, ScopeError, SecureEntityExt, secure_insert};
use toolkit_db::{ConnectOpts, connect_db};
use toolkit_security::AccessScope;
use uuid::Uuid;

const TENANT_A: Uuid = Uuid::from_u128(0xA);
const TENANT_B: Uuid = Uuid::from_u128(0xB);

// ════════════════════════════════════════════════════════════════════
// Entities — the composite-key graph
// ════════════════════════════════════════════════════════════════════

mod node {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "pgq_node")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    impl toolkit_db::secure::ScopableEntity for Entity {
        fn tenant_col() -> Option<Column> {
            Some(Column::TenantId)
        }
        fn resource_col() -> Option<Column> {
            Some(Column::Id)
        }
        fn owner_col() -> Option<Column> {
            None
        }
        fn type_col() -> Option<Column> {
            None
        }
        fn resolve_property(property: &str) -> Option<Column> {
            match property {
                p if p == toolkit_security::pep_properties::OWNER_TENANT_ID => {
                    Some(Column::TenantId)
                }
                p if p == toolkit_security::pep_properties::RESOURCE_ID => Some(Column::Id),
                _ => None,
            }
        }
        fn scope_columns() -> Vec<Column> {
            vec![Column::TenantId, Column::Id]
        }
    }
}

mod edge {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "pgq_edge")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        pub src_id: i64,
        pub dst_id: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    impl toolkit_db::secure::ScopableEntity for Entity {
        fn tenant_col() -> Option<Column> {
            Some(Column::TenantId)
        }
        fn resource_col() -> Option<Column> {
            Some(Column::Id)
        }
        fn owner_col() -> Option<Column> {
            None
        }
        fn type_col() -> Option<Column> {
            None
        }
        fn resolve_property(property: &str) -> Option<Column> {
            match property {
                p if p == toolkit_security::pep_properties::OWNER_TENANT_ID => {
                    Some(Column::TenantId)
                }
                p if p == toolkit_security::pep_properties::RESOURCE_ID => Some(Column::Id),
                _ => None,
            }
        }
        fn scope_columns() -> Vec<Column> {
            vec![Column::TenantId, Column::Id]
        }
    }
}

struct Kb;

impl PropertyGraph for Kb {
    const GRAPH_NAME: &'static str = "pgq_kb";

    fn declaration() -> Result<GraphDeclaration, ScopeError> {
        GraphDeclaration::new::<Self>()
            .vertex::<Self, node::Entity>(&["tenant_id", "id"])?
            .edge::<Self, edge::Entity>(
                &["tenant_id", "id"],
                Endpoint {
                    key: vec!["tenant_id".to_owned(), "src_id".to_owned()],
                    table: "pgq_node".to_owned(),
                    references: vec!["tenant_id".to_owned(), "id".to_owned()],
                },
                Endpoint {
                    key: vec!["tenant_id".to_owned(), "dst_id".to_owned()],
                    table: "pgq_node".to_owned(),
                    references: vec!["tenant_id".to_owned(), "id".to_owned()],
                },
            )
    }
}

impl VertexOf<Kb> for node::Entity {
    const LABEL: &'static str = "node";
}

impl toolkit_db::secure::pgq::EdgeOf<Kb> for edge::Entity {
    const LABEL: &'static str = "edge";
}

// ════════════════════════════════════════════════════════════════════
// Entities — the open (single-column-key) graph
// ════════════════════════════════════════════════════════════════════

mod open_node {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "pgq_open_node")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        pub tenant_id: Uuid,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    impl toolkit_db::secure::ScopableEntity for Entity {
        fn tenant_col() -> Option<Column> {
            Some(Column::TenantId)
        }
        fn resource_col() -> Option<Column> {
            Some(Column::Id)
        }
        fn owner_col() -> Option<Column> {
            None
        }
        fn type_col() -> Option<Column> {
            None
        }
        fn resolve_property(property: &str) -> Option<Column> {
            match property {
                p if p == toolkit_security::pep_properties::OWNER_TENANT_ID => {
                    Some(Column::TenantId)
                }
                p if p == toolkit_security::pep_properties::RESOURCE_ID => Some(Column::Id),
                _ => None,
            }
        }
        fn scope_columns() -> Vec<Column> {
            vec![Column::TenantId, Column::Id]
        }
    }
}

mod open_edge {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "pgq_open_edge")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        pub tenant_id: Uuid,
        pub src_id: i64,
        pub dst_id: i64,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    impl toolkit_db::secure::ScopableEntity for Entity {
        fn tenant_col() -> Option<Column> {
            Some(Column::TenantId)
        }
        fn resource_col() -> Option<Column> {
            Some(Column::Id)
        }
        fn owner_col() -> Option<Column> {
            None
        }
        fn type_col() -> Option<Column> {
            None
        }
        fn resolve_property(property: &str) -> Option<Column> {
            match property {
                p if p == toolkit_security::pep_properties::OWNER_TENANT_ID => {
                    Some(Column::TenantId)
                }
                p if p == toolkit_security::pep_properties::RESOURCE_ID => Some(Column::Id),
                _ => None,
            }
        }
        fn scope_columns() -> Vec<Column> {
            vec![Column::TenantId, Column::Id]
        }
    }
}

struct OpenKb;

impl PropertyGraph for OpenKb {
    const GRAPH_NAME: &'static str = "pgq_open";

    fn declaration() -> Result<GraphDeclaration, ScopeError> {
        GraphDeclaration::new::<Self>()
            .vertex::<Self, open_node::Entity>(&["id"])?
            .edge::<Self, open_edge::Entity>(
                &["id"],
                Endpoint {
                    key: vec!["src_id".to_owned()],
                    table: "pgq_open_node".to_owned(),
                    references: vec!["id".to_owned()],
                },
                Endpoint {
                    key: vec!["dst_id".to_owned()],
                    table: "pgq_open_node".to_owned(),
                    references: vec!["id".to_owned()],
                },
            )
    }
}

impl VertexOf<OpenKb> for open_node::Entity {
    const LABEL: &'static str = "node";
}

impl toolkit_db::secure::pgq::EdgeOf<OpenKb> for open_edge::Entity {
    const LABEL: &'static str = "edge";
}

// ════════════════════════════════════════════════════════════════════
// Schema
// ════════════════════════════════════════════════════════════════════

struct CreateTables;

impl mig::MigrationName for CreateTables {
    fn name(&self) -> &'static str {
        "pgq_create_tables"
    }
}

fn bigint_col(name: &str) -> mig::ColumnDef {
    let mut col = mig::ColumnDef::new(mig::Alias::new(name));
    col.big_integer().not_null();
    col
}

fn uuid_col(name: &str) -> mig::ColumnDef {
    let mut col = mig::ColumnDef::new(mig::Alias::new(name));
    col.uuid().not_null();
    col
}

fn string_col(name: &str) -> mig::ColumnDef {
    let mut col = mig::ColumnDef::new(mig::Alias::new(name));
    col.string().not_null();
    col
}

#[async_trait::async_trait]
impl mig::MigrationTrait for CreateTables {
    async fn up(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        manager
            .create_table(
                mig::Table::create()
                    .table(mig::Alias::new("pgq_node"))
                    .col(uuid_col("tenant_id"))
                    .col(bigint_col("id"))
                    .col(string_col("name"))
                    .primary_key(
                        mig::Index::create()
                            .col(mig::Alias::new("tenant_id"))
                            .col(mig::Alias::new("id")),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                mig::Table::create()
                    .table(mig::Alias::new("pgq_edge"))
                    .col(uuid_col("tenant_id"))
                    .col(bigint_col("id"))
                    .col(bigint_col("src_id"))
                    .col(bigint_col("dst_id"))
                    .primary_key(
                        mig::Index::create()
                            .col(mig::Alias::new("tenant_id"))
                            .col(mig::Alias::new("id")),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                mig::Table::create()
                    .table(mig::Alias::new("pgq_open_node"))
                    .col(bigint_col("id"))
                    .col(uuid_col("tenant_id"))
                    .col(string_col("name"))
                    .primary_key(mig::Index::create().col(mig::Alias::new("id")))
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(
                mig::Table::create()
                    .table(mig::Alias::new("pgq_open_edge"))
                    .col(bigint_col("id"))
                    .col(uuid_col("tenant_id"))
                    .col(bigint_col("src_id"))
                    .col(bigint_col("dst_id"))
                    .primary_key(mig::Index::create().col(mig::Alias::new("id")))
                    .to_owned(),
            )
            .await?;

        // The property graphs are schema objects, and the DDL is the *generated*
        // one — so these statements are also the assertion that what the
        // declarations emit is what PostgreSQL accepts. Raw SQL is the
        // established idiom for statements sea-orm-migration cannot model.
        for ddl in [
            Kb::declaration()
                .map_err(|e| mig::DbErr::Custom(format!("declaration: {e}")))?
                .create_statement()
                .map_err(|e| mig::DbErr::Custom(format!("ddl: {e}")))?,
            OpenKb::declaration()
                .map_err(|e| mig::DbErr::Custom(format!("declaration: {e}")))?
                .create_statement()
                .map_err(|e| mig::DbErr::Custom(format!("ddl: {e}")))?,
        ] {
            manager.get_connection().execute_unprepared(&ddl).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &mig::SchemaManager) -> Result<(), mig::DbErr> {
        for drop in [
            Kb::declaration()
                .map_err(|e| mig::DbErr::Custom(format!("declaration: {e}")))?
                .drop_statement()
                .map_err(|e| mig::DbErr::Custom(format!("ddl: {e}")))?,
            OpenKb::declaration()
                .map_err(|e| mig::DbErr::Custom(format!("declaration: {e}")))?
                .drop_statement()
                .map_err(|e| mig::DbErr::Custom(format!("ddl: {e}")))?,
        ] {
            manager.get_connection().execute_unprepared(&drop).await?;
        }
        Ok(())
    }
}

// ════════════════════════════════════════════════════════════════════
// Harness
// ════════════════════════════════════════════════════════════════════

struct Stand {
    db: Db,
    dsn: String,
    _container: ContainerAsync<Postgres>,
}

/// Bring up `PostgreSQL` 19, or explain why not.
///
/// `None` means skip. While the image is pre-GA that is the default;
/// `GEARS_TEST_PG_GRAPH_REQUIRED=1` makes an unavailable image a failure, so a
/// lane that is supposed to cover PG19 cannot pass by skipping.
async fn stand() -> Option<Stand> {
    let request = test_containers::postgres_graph()
        .with_env_var("POSTGRES_PASSWORD", "pass")
        .with_env_var("POSTGRES_USER", "user")
        .with_env_var("POSTGRES_DB", "app");

    let container = match request.start().await {
        Ok(container) => container,
        Err(error) => {
            assert!(
                !test_containers::graph_lane_required(),
                "GEARS_TEST_PG_GRAPH_REQUIRED is set but PostgreSQL 19 \
                 ({}) could not start: {error}",
                test_containers::postgres_graph_tag()
            );
            eprintln!("PostgreSQL 19 unavailable - skipping the SQL/PGQ lane: {error}");
            return None;
        }
    };

    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let dsn = format!("postgres://user:pass@127.0.0.1:{port}/app");
    let db = connect_db(
        &dsn,
        ConnectOpts {
            max_conns: Some(4),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect");

    run_migrations_for_testing(&db, vec![Box::new(CreateTables)])
        .await
        .expect("migrate");

    Some(Stand {
        db,
        dsn,
        _container: container,
    })
}

impl Stand {
    fn conn(&self) -> DbConn<'_> {
        self.db.conn().expect("conn")
    }

    /// The composite-key fixture.
    ///
    /// Tenant A: `1 -> 2` (edge 100) and `1 -> 11` (edge 300 — both tenants own
    /// a node `11`, so a cross-tenant leak surfaces as an *extra* `11`).
    /// Tenant B: `10 -> 11` (edge 200). Edge 301 is the structural trap: its
    /// destination id `10` exists only in B, and the composite endpoint key
    /// must keep it unresolvable in every query, scoped or not.
    async fn seed(&self) {
        let conn = self.conn();
        let unrestricted = AccessScope::allow_all();

        for (tenant, id, name) in [
            (TENANT_A, 1_i64, "a1"),
            (TENANT_A, 2, "a2"),
            (TENANT_A, 11, "a11"),
            (TENANT_B, 10, "b10"),
            (TENANT_B, 11, "b11"),
        ] {
            secure_insert::<node::Entity>(
                node::ActiveModel {
                    tenant_id: Set(tenant),
                    id: Set(id),
                    name: Set(name.to_owned()),
                },
                &unrestricted,
                &conn,
            )
            .await
            .expect("insert node");
        }

        for (tenant, id, src, dst) in [
            (TENANT_A, 100_i64, 1_i64, 2_i64),
            (TENANT_B, 200, 10, 11),
            // Resolvable within A now that A owns a node 11: the walk this edge
            // carries must appear for A and must not multiply for B.
            (TENANT_A, 300, 1, 11),
            // The structural trap: no node (A, 10) exists, so the composite
            // endpoint key must make this edge yield no match anywhere.
            (TENANT_A, 301, 2, 10),
        ] {
            secure_insert::<edge::Entity>(
                edge::ActiveModel {
                    tenant_id: Set(tenant),
                    id: Set(id),
                    src_id: Set(src),
                    dst_id: Set(dst),
                },
                &unrestricted,
                &conn,
            )
            .await
            .expect("insert edge");
        }
    }

    /// The open fixture: node ids are globally unique and edge endpoints key on
    /// `id` alone, so an edge *can* resolve across a tenant boundary.
    ///
    /// Tenant A: `1 -> 2` (edge 100). Tenant B: `10 -> 11` (edge 200), plus the
    /// two crossings — edge 400 is **B's** edge between **A's** nodes `1 -> 2`,
    /// and edge 300 is A's edge into B's node (`1 -> 11`).
    async fn seed_open(&self) {
        let conn = self.conn();
        let unrestricted = AccessScope::allow_all();

        for (tenant, id, name) in [
            (TENANT_A, 1_i64, "a1"),
            (TENANT_A, 2, "a2"),
            (TENANT_B, 10, "b10"),
            (TENANT_B, 11, "b11"),
        ] {
            secure_insert::<open_node::Entity>(
                open_node::ActiveModel {
                    id: Set(id),
                    tenant_id: Set(tenant),
                    name: Set(name.to_owned()),
                },
                &unrestricted,
                &conn,
            )
            .await
            .expect("insert open node");
        }

        for (tenant, id, src, dst) in [
            (TENANT_A, 100_i64, 1_i64, 2_i64),
            (TENANT_B, 200, 10, 11),
            (TENANT_B, 400, 1, 2),
            (TENANT_A, 300, 1, 11),
        ] {
            secure_insert::<open_edge::Entity>(
                open_edge::ActiveModel {
                    id: Set(id),
                    tenant_id: Set(tenant),
                    src_id: Set(src),
                    dst_id: Set(dst),
                },
                &unrestricted,
                &conn,
            )
            .await
            .expect("insert open edge");
        }
    }

    /// A plain connection, for the deliberately unscoped control queries only.
    async fn raw(&self) -> sea_orm::DatabaseConnection {
        sea_orm::Database::connect(&self.dsn)
            .await
            .expect("raw connect")
    }
}

#[derive(Debug, sea_orm::FromQueryResult)]
struct Neighbour {
    neighbour: i64,
}

/// Raw, sorted, **non-deduplicated** ids: row multiplication and leaked
/// duplicates are signal here, not noise.
fn ids(rows: Vec<Neighbour>) -> Vec<i64> {
    let mut ids: Vec<i64> = rows.into_iter().map(|r| r.neighbour).collect();
    ids.sort_unstable();
    ids
}

// ════════════════════════════════════════════════════════════════════
// Tests — the composite-key graph
// ════════════════════════════════════════════════════════════════════

/// The preconditions every test below rests on: the fixture rows whose absence
/// would turn a guard test into a vacuous pass. Asserted separately so a
/// fixture that loses them fails here, loudly.
#[tokio::test]
async fn the_fixture_traps_exist() {
    let Some(stand) = stand().await else { return };
    stand.seed().await;

    let conn = stand.conn();
    let all = AccessScope::allow_all();

    // A's node 11 mirrors B's node 11: with both present, a leaked B-walk shows
    // up as a duplicate 11 in an A-scoped result.
    let mirrored = node::Entity::find()
        .secure()
        .scope_with(&all)
        .filter(sea_orm::Condition::all().add(node::Column::Id.eq(11_i64)))
        .all(&conn)
        .await
        .expect("query");
    assert_eq!(
        mirrored.len(),
        2,
        "both tenants must own a node 11, or a cross-tenant leak has no \
         duplicate to surface as"
    );

    // The structurally unresolvable edge is really in the table.
    let unresolvable = edge::Entity::find()
        .secure()
        .scope_with(&all)
        .filter(
            sea_orm::Condition::all()
                .add(edge::Column::TenantId.eq(TENANT_A))
                .add(edge::Column::DstId.eq(10_i64)),
        )
        .all(&conn)
        .await
        .expect("query");
    assert_eq!(
        unresolvable.len(),
        1,
        "the unresolvable trap edge is missing; the composite-key assertion \
         below would pass without proving anything"
    );
}

/// A scoped walk stays inside the caller's tenant — asserted on the raw row
/// list, so a leaked or multiplied row cannot hide behind a dedup.
#[tokio::test]
async fn a_scoped_hop_does_not_leave_the_tenant() {
    let Some(stand) = stand().await else { return };
    stand.seed().await;

    let conn = stand.conn();
    let rows: Vec<Neighbour> = node::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(TENANT_A))
        .with_graph::<Kb>()
        .match_path(|p| {
            p.vertex::<node::Entity>("a")
                .edge_to::<edge::Entity>("e")
                .to::<node::Entity>("b")
        })
        .column("b", "id", "neighbour")
        .all_as(&conn)
        .await
        .expect("graph query");

    assert_eq!(
        ids(rows),
        vec![2, 11],
        "tenant A reaches 2 and its own 11, each exactly once; an extra 11 \
         would be tenant B's row"
    );
}

/// Tenant B sees its own graph and nothing of A's.
#[tokio::test]
async fn each_tenant_sees_only_its_own_graph() {
    let Some(stand) = stand().await else { return };
    stand.seed().await;

    let conn = stand.conn();
    let rows: Vec<Neighbour> = node::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(TENANT_B))
        .with_graph::<Kb>()
        .match_path(|p| {
            p.vertex::<node::Entity>("a")
                .edge_to::<edge::Entity>("e")
                .to::<node::Entity>("b")
        })
        .column("b", "id", "neighbour")
        .all_as(&conn)
        .await
        .expect("graph query");

    assert_eq!(ids(rows), vec![11]);
}

/// A deny-all scope returns nothing rather than everything.
#[tokio::test]
async fn deny_all_returns_no_rows() {
    let Some(stand) = stand().await else { return };
    stand.seed().await;

    let conn = stand.conn();
    let rows: Vec<Neighbour> = node::Entity::find()
        .secure()
        .scope_with(&AccessScope::deny_all())
        .with_graph::<Kb>()
        .match_path(|p| {
            p.vertex::<node::Entity>("a")
                .edge_to::<edge::Entity>("e")
                .to::<node::Entity>("b")
        })
        .column("b", "id", "neighbour")
        .all_as(&conn)
        .await
        .expect("graph query");

    assert!(rows.is_empty(), "deny-all must return nothing: {rows:?}");
}

/// The "start from these rows, walk out one hop" shape: an element predicate
/// correlates the pattern with the anchor query, and the anchor's rows steer
/// the walk. Verified by narrowing the anchor: starting from node 1 reaches its
/// two neighbours; starting from node 2 reaches nothing — which also shows the
/// correlation is real, not a cross join that would return the same rows for
/// any anchor.
#[tokio::test]
async fn an_anchored_walk_starts_from_the_anchor_rows() {
    async fn from(conn: &DbConn<'_>, start: i64) -> Vec<i64> {
        use sea_orm::sea_query::{Alias, Expr};
        let rows: Vec<Neighbour> =
            node::Entity::find()
                .secure()
                .scope_with(&AccessScope::for_tenant(TENANT_A))
                .with_graph::<Kb>()
                .match_path(|p| {
                    p.vertex::<node::Entity>("a")
                        .where_(
                            sea_orm::Condition::all()
                                .add(Expr::col((Alias::new("a"), Alias::new("tenant_id"))).eq(
                                    Expr::col((Alias::new("pgq_node"), Alias::new("tenant_id"))),
                                ))
                                .add(
                                    Expr::col((Alias::new("a"), Alias::new("id")))
                                        .eq(Expr::col((Alias::new("pgq_node"), Alias::new("id")))),
                                ),
                        )
                        .correlate_with_anchor()
                        .edge_to::<edge::Entity>("e")
                        .to::<node::Entity>("b")
                })
                .column("b", "id", "neighbour")
                .filter(sea_orm::Condition::all().add(node::Column::Id.eq(start)))
                .all_as(conn)
                .await
                .expect("anchored graph query");
        ids(rows)
    }

    let Some(stand) = stand().await else { return };
    stand.seed().await;
    let conn = stand.conn();

    assert_eq!(
        from(&conn, 1).await,
        vec![2, 11],
        "node 1 has two outgoing hops"
    );
    assert_eq!(
        from(&conn, 2).await,
        Vec::<i64>::new(),
        "node 2 has no resolvable outgoing edge (301's destination is only B's)"
    );
}

/// The control: **can this suite see a hole at all?**
///
/// The same pattern with no scope anywhere, built from the syntax crate (which
/// embeds none) and run over a plain connection. It must return both tenants'
/// walks — including B's `11`, which an A-scoped query must not have — and it
/// must **not** contain `10`: edge 301's destination key `(A, 10)` matches no
/// node row, which is the composite endpoint key structurally refusing a
/// cross-tenant join before any predicate applies.
#[tokio::test]
async fn the_unscoped_pattern_does_reach_across_tenants() {
    let Some(stand) = stand().await else { return };
    stand.seed().await;

    let unscoped = toolkit_sea_orm_pgq::GraphTable::new(
        Kb::GRAPH_NAME,
        toolkit_sea_orm_pgq::GraphPattern::new(toolkit_sea_orm_pgq::Element::new("a", "node")).hop(
            toolkit_sea_orm_pgq::Element::new("e", "edge"),
            toolkit_sea_orm_pgq::Direction::Outgoing,
            toolkit_sea_orm_pgq::Element::new("b", "node"),
        ),
    )
    .column(toolkit_sea_orm_pgq::ProjectedColumn::new(
        "b",
        "id",
        "neighbour",
    ));

    let statement = sea_orm::sea_query::Query::select()
        .expr_as(
            sea_orm::sea_query::Expr::col((
                sea_orm::sea_query::Alias::new("g"),
                sea_orm::sea_query::Alias::new("neighbour"),
            )),
            sea_orm::sea_query::Alias::new("neighbour"),
        )
        .from(unscoped.into_table_ref("g").expect("renders"))
        .to_owned();
    let stmt = sea_orm::StatementBuilder::build(&statement, &sea_orm::DbBackend::Postgres);

    let raw = stand.raw().await;
    let rows = Neighbour::find_by_statement(stmt)
        .all(&raw)
        .await
        .expect("unscoped query");

    assert_eq!(
        ids(rows),
        vec![2, 11, 11],
        "unscoped, both tenants' walks are visible (the second 11 is B's); \
         and no 10, because the composite endpoint key must keep edge 301 unresolvable"
    );
}

// ════════════════════════════════════════════════════════════════════
// Tests — the open graph, where one unscoped element is observable
// ════════════════════════════════════════════════════════════════════

/// On the open graph a fully scoped walk returns each of tenant A's own hops
/// exactly once: A's edge into B's node 11 is cut by the target's scope, and
/// B's edge 400 between A's nodes is cut by the edge's own scope.
#[tokio::test]
async fn open_graph_scoped_walk_returns_own_rows_once() {
    let Some(stand) = stand().await else { return };
    stand.seed_open().await;

    let conn = stand.conn();
    let rows: Vec<Neighbour> = open_node::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(TENANT_A))
        .with_graph::<OpenKb>()
        .match_path(|p| {
            p.vertex::<open_node::Entity>("a")
                .edge_to::<open_edge::Entity>("e")
                .to::<open_node::Entity>("b")
        })
        .column("b", "id", "neighbour")
        .all_as(&conn)
        .await
        .expect("open graph query");

    assert_eq!(
        ids(rows),
        vec![2],
        "one hop, once: a second 2 would be B's edge 400, an 11 would be A's \
         edge into B's node"
    );
}

/// The per-element control: scope on **both vertices**, none on the edge body,
/// over a fixture whose crossing edges resolve. B's edge 400 connects A's two
/// nodes, so the edge element — the only unscoped one — admits a foreign row,
/// visible as the second `2`. The fully scoped variant above returns one.
/// This is what shows an *element-level* hole is observable at all; the
/// whole-pattern control cannot, because every element leaks at once there.
#[tokio::test]
async fn an_unscoped_edge_element_admits_foreign_rows() {
    use sea_orm::sea_query::{Alias, Expr};

    let Some(stand) = stand().await else { return };
    stand.seed_open().await;

    let scoped_vertex = |var: &'static str| {
        toolkit_sea_orm_pgq::Element::new(var, "node").and_where(
            sea_orm::Condition::all()
                .add(Expr::col((Alias::new(var), Alias::new("tenant_id"))).eq(TENANT_A)),
        )
    };

    let control = toolkit_sea_orm_pgq::GraphTable::new(
        OpenKb::GRAPH_NAME,
        toolkit_sea_orm_pgq::GraphPattern::new(scoped_vertex("a")).hop(
            // The deliberate hole: no scope on the edge body.
            toolkit_sea_orm_pgq::Element::new("e", "edge"),
            toolkit_sea_orm_pgq::Direction::Outgoing,
            scoped_vertex("b"),
        ),
    )
    .column(toolkit_sea_orm_pgq::ProjectedColumn::new(
        "b",
        "id",
        "neighbour",
    ));

    let statement = sea_orm::sea_query::Query::select()
        .expr_as(
            Expr::col((Alias::new("g"), Alias::new("neighbour"))),
            Alias::new("neighbour"),
        )
        .from(control.into_table_ref("g").expect("renders"))
        .to_owned();
    let stmt = sea_orm::StatementBuilder::build(&statement, &sea_orm::DbBackend::Postgres);

    let raw = stand.raw().await;
    let rows = Neighbour::find_by_statement(stmt)
        .all(&raw)
        .await
        .expect("control query");

    assert_eq!(
        ids(rows),
        vec![2, 2],
        "with the edge body unscoped, B's edge 400 must carry a second walk \
         between A's vertices; if it does not, the scoped test above proves \
         nothing about edge scope"
    );
}

// ════════════════════════════════════════════════════════════════════
// Declaration-level behaviour on a live server
// ════════════════════════════════════════════════════════════════════

/// A column absent from the declaration's `PROPERTIES` list is invisible to
/// `MATCH` — the quiet failure Policy 3 exists to prevent. Here it is made
/// loud: the server refuses the pattern rather than silently ignoring it.
/// (The secure builder refuses the same projection at build time; this pins
/// the server-side behaviour the build-time check models.)
#[tokio::test]
async fn a_column_outside_properties_cannot_be_matched_on() {
    let Some(stand) = stand().await else { return };
    stand.seed().await;

    // `name` is a real column of pgq_node and is deliberately not exposed: the
    // declaration lists only the key and the scope columns.
    let declaration = Kb::declaration().expect("declares");
    let exposed = declaration.properties_of("node").expect("node");
    assert!(
        !exposed.contains(&"name".to_owned()),
        "this test needs an unexposed column: {exposed:?}"
    );

    let table = toolkit_sea_orm_pgq::GraphTable::new(
        Kb::GRAPH_NAME,
        toolkit_sea_orm_pgq::GraphPattern::new(toolkit_sea_orm_pgq::Element::new("a", "node")),
    )
    .column(toolkit_sea_orm_pgq::ProjectedColumn::new("a", "name", "n"));

    let statement = sea_orm::sea_query::Query::select()
        .expr(sea_orm::sea_query::Expr::val(1))
        .from(table.into_table_ref("g").expect("renders"))
        .to_owned();
    let raw = stand.raw().await;
    let outcome = raw.query_all(&statement).await;
    assert!(
        outcome.is_err(),
        "projecting an unexposed column must be refused, not silently answered"
    );
}
