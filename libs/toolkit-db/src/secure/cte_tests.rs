//! Tests for CTE support in the Secure ORM.
//!
//! These assert on the **rendered SQL, per backend**, rather than on the `Debug`
//! form of a `sea_orm::Condition` (the style used in `cond.rs`). The isolation
//! claim in ADR-0001 is about what reaches the database, so that is what gets
//! asserted; a `Condition` that never makes it into the emitted statement would
//! satisfy a Debug-string check and still leak.

use super::*;
use sea_orm::DbBackend;

/// Every backend the Secure ORM renders for.
const BACKENDS: [DbBackend; 3] = [DbBackend::Postgres, DbBackend::MySql, DbBackend::Sqlite];

/// A tenant-scoped entity with a self-referencing `parent_id`, so the same
/// fixture serves both the flat and the recursive tests.
mod node {
    use sea_orm::entity::prelude::*;
    use toolkit_security::access_scope::pep_properties;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "cte_node")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub parent_id: Option<Uuid>,
        pub name: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    impl crate::secure::ScopableEntity for Entity {
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
                p if p == pep_properties::OWNER_TENANT_ID => Some(Column::TenantId),
                p if p == pep_properties::RESOURCE_ID => Some(Column::Id),
                _ => None,
            }
        }
        fn scope_columns() -> Vec<Column> {
            vec![Column::TenantId, Column::Id]
        }
    }
}

/// A second scoped entity, for asserting that *every* CTE body is scoped rather
/// than just the first.
mod item {
    use sea_orm::entity::prelude::*;
    use toolkit_security::access_scope::pep_properties;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "cte_item")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub node_id: Uuid,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    impl crate::secure::ScopableEntity for Entity {
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
                p if p == pep_properties::OWNER_TENANT_ID => Some(Column::TenantId),
                p if p == pep_properties::RESOURCE_ID => Some(Column::Id),
                _ => None,
            }
        }
        fn scope_columns() -> Vec<Column> {
            vec![Column::TenantId, Column::Id]
        }
    }
}

fn tenant_scope(tenant: uuid::Uuid) -> AccessScope {
    AccessScope::for_tenants(vec![tenant])
}

/// A scoped select over `node`, the starting point for every test below.
fn scoped_nodes(scope: &AccessScope) -> SecureSelect<node::Entity, Scoped> {
    node::Entity::find().secure().scope_with(scope)
}

/// The text between `AS (` and its *matching* `)`, i.e. one CTE body alone.
///
/// Naive `split_once("AS (")` keeps the trailing outer query in the tail, so any
/// assertion about "the body" would silently also see the outer query's
/// predicates -- which makes a missing predicate inside the CTE invisible.
fn cte_body(sql: &str) -> &str {
    let open = sql.find("AS (").expect("no CTE in statement") + "AS (".len();
    let mut depth = 1usize;
    for (offset, ch) in sql[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &sql[open..open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced parentheses in {sql}")
}

/// Occurrences of a scope predicate. Deliberately matches `tenant_id IN (` and
/// not bare `tenant_id`: the column also appears in every SELECT list, so a
/// substring check on the name alone passes even against an unscoped query.
fn scope_predicates(fragment: &str) -> usize {
    fragment.matches("tenant_id\" IN (").count() + fragment.matches("tenant_id` IN (").count()
}

#[test]
fn cte_body_embeds_scope_condition_on_every_backend() {
    // The load-bearing invariant of ADR-0001: the CTE body carries its own scope
    // predicate, because the outer WHERE cannot reach inside WITH.
    let tenant = uuid::Uuid::new_v4();
    let scope = tenant_scope(tenant);

    for backend in BACKENDS {
        let stmt = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>("scoped_items", |q| q)
            .build_statement(backend);
        let sql = stmt.to_string();

        assert_eq!(
            scope_predicates(cte_body(&sql)),
            1,
            "{backend:?} CTE body has no tenant predicate: {sql}"
        );
        assert!(
            sql.contains(&tenant.to_string()),
            "{backend:?} did not bind the tenant id: {sql}"
        );
    }
}

#[test]
fn with_ctes_emits_with_clause_on_every_backend() {
    // Regression guard named in the ADR: if this type ever fell back to the plain
    // `Select<E>` path, the WITH clause would silently disappear and the query
    // would still run -- just against unfiltered tables.
    let scope = tenant_scope(uuid::Uuid::new_v4());

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>("scoped_items", |q| q)
            .build_statement(backend)
            .to_string();
        assert!(
            sql.trim_start().starts_with("WITH "),
            "{backend:?} dropped the WITH clause: {sql}"
        );
    }
}

#[test]
fn every_cte_body_is_scoped_not_just_the_first() {
    let tenant = uuid::Uuid::new_v4();
    let scope = tenant_scope(tenant);

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>("a", |q| q)
            .cte::<node::Entity>("b", |q| q)
            .build_statement(backend)
            .to_string();

        assert!(sql.contains("cte_item"), "{backend:?}: {sql}");
        assert!(sql.contains("cte_node"), "{backend:?}: {sql}");
        // Exactly three scope predicates: one per CTE body, one for the outer
        // query. Counted on the `IN (` form, so SELECT-list mentions of the
        // column do not inflate it -- otherwise dropping a body's scope entirely
        // would still leave the count above any `>=` threshold.
        assert_eq!(
            scope_predicates(&sql),
            3,
            "{backend:?} expected a tenant predicate per body plus the outer query: {sql}"
        );
    }
}

#[test]
fn cte_body_with_deny_all_scope_denies() {
    // Error path: a deny-all scope must produce a body that selects nothing,
    // rather than an unfiltered body.
    let scope = AccessScope::default();
    assert!(scope.is_deny_all(), "precondition");

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>("scoped_items", |q| q)
            .build_statement(backend)
            .to_string();
        let body = cte_body(&sql);
        assert!(
            body.contains("FALSE") || body.contains("0 = 1") || body.contains("false"),
            "{backend:?} deny-all body is not a deny: {sql}"
        );
    }
}

#[test]
fn cte_body_with_unconstrained_scope_carries_no_tenant_predicate() {
    // Negative space, and it pins allow-all semantics. `build_scope_condition`
    // returns an empty `Condition::all()` for an unconstrained scope, which
    // sea_query renders as a literal `WHERE TRUE` -- not as an absent WHERE. So
    // the assertion is about the *predicate*, not the keyword: allow-all must
    // not filter, and must not accidentally deny either.
    let scope = AccessScope::allow_all();

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>("scoped_items", |q| q)
            .build_statement(backend)
            .to_string();
        let body = cte_body(&sql);

        assert_eq!(
            scope_predicates(body),
            0,
            "{backend:?} allow-all body should not filter by tenant: {sql}"
        );
        assert!(
            !body.contains("FALSE"),
            "{backend:?} allow-all must not deny: {sql}"
        );
    }
}

#[test]
fn cte_name_with_sql_metacharacters_renders_as_one_quoted_identifier() {
    // The ADR claims a hostile CTE name cannot inject SQL because sea_query
    // renders it as a quoted, escaped identifier. In sea-query 1.0.2 that holds
    // via `is_static_iden`: anything outside [A-Za-z_][A-Za-z0-9_]* falls to the
    // owned branch of `prepare_iden`, which doubles the quote char.
    let scope = tenant_scope(uuid::Uuid::new_v4());
    let hostile = r#"foo"; DROP TABLE x --"#;

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>(hostile, |q| q)
            .build_statement(backend)
            .to_string();

        let expected = match backend {
            // `"` doubled, so the embedded quote cannot terminate the identifier.
            DbBackend::Postgres | DbBackend::Sqlite => r#""foo""; DROP TABLE x --""#,
            // MySQL quotes with backticks, so a `"` in the payload is ordinary.
            DbBackend::MySql => "`foo\"; DROP TABLE x --`",
            // `DbBackend` is #[non_exhaustive]; BACKENDS holds only the three above.
            _ => unreachable!("unexpected backend {backend:?}"),
        };

        // Structural, not just "contains": the name must occupy exactly the
        // identifier slot of a single CTE definition. If the payload had broken
        // out of its quoting, the prefix would not line up and `AS (` would land
        // somewhere else.
        assert_eq!(
            sql.matches(" AS (").count(),
            1,
            "{backend:?} name escaped its identifier slot: {sql}"
        );
        assert!(
            sql.starts_with(&format!("WITH {expected} AS (")),
            "{backend:?} expected the name as one quoted identifier {expected}, got: {sql}"
        );
    }
}

#[test]
fn cte_name_with_backtick_is_escaped_for_mysql() {
    // Companion to the test above: the doubling rule is per-backend, so the
    // MySQL quote char needs its own payload to be meaningful.
    let scope = tenant_scope(uuid::Uuid::new_v4());
    let sql = scoped_nodes(&scope)
        .with_ctes()
        .cte::<item::Entity>("ev`il", |q| q)
        .build_statement(DbBackend::MySql)
        .to_string();

    assert!(sql.contains("`ev``il`"), "backtick not doubled: {sql}");
}

#[test]
fn join_cte_references_the_cte_by_quoted_name() {
    // `with_ctes` only defines; without this the WITH clause computes nothing.
    let scope = tenant_scope(uuid::Uuid::new_v4());

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>("scoped_items", |q| q)
            .join_cte(
                "scoped_items",
                Condition::all().add(
                    Expr::col((Alias::new("scoped_items"), Alias::new("node_id")))
                        .equals((node::Entity, node::Column::Id)),
                ),
            )
            .build_statement(backend)
            .to_string();

        assert!(
            sql.contains("JOIN"),
            "{backend:?} did not reference the CTE: {sql}"
        );
        assert!(
            sql.matches("scoped_items").count() >= 2,
            "{backend:?} CTE should appear as definition and as source: {sql}"
        );
    }
}

#[test]
fn build_statement_binds_scope_values_instead_of_interpolating_them() {
    // Guards against a regression to string interpolation. The tenant UUID must
    // travel as a bound parameter, not spliced into the SQL text.
    let tenant = uuid::Uuid::new_v4();
    let scope = tenant_scope(tenant);

    for backend in BACKENDS {
        let stmt = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>("scoped_items", |q| q)
            .build_statement(backend);

        let values = stmt.values.as_ref().expect("bound values");
        assert!(!values.0.is_empty(), "{backend:?} bound nothing");
        assert!(
            !stmt.sql.contains(&tenant.to_string()),
            "{backend:?} interpolated the tenant id into SQL: {}",
            stmt.sql
        );
    }
}

#[test]
fn cte_body_filter_is_anded_with_scope_not_replacing_it() {
    // A caller-supplied body filter must not be able to displace the scope
    // predicate; both have to survive.
    let tenant = uuid::Uuid::new_v4();
    let scope = tenant_scope(tenant);

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .cte::<node::Entity>("named", |q| {
                q.filter(node::Column::Name.eq("keep-me".to_owned()))
            })
            .build_statement(backend)
            .to_string();

        let body = cte_body(&sql);
        assert!(body.contains("name"), "{backend:?} lost body filter: {sql}");
        assert_eq!(
            scope_predicates(body),
            1,
            "{backend:?} body filter displaced the scope: {sql}"
        );
    }
}

#[test]
fn outer_filter_is_anded_with_the_outer_scope_not_replacing_it() {
    // `SecureCteSelect::filter` writes to a `sea_query::SelectStatement` that
    // already carries the scope `WHERE` from `scope_with`. sea_query's
    // `cond_where` merges into the existing `Condition::all()` rather than
    // overwriting it, but that is a property of a dependency, so pin it: a
    // regression here would silently drop the outer query's scope.
    let tenant = uuid::Uuid::new_v4();
    let scope = tenant_scope(tenant);
    let target = uuid::Uuid::new_v4();

    for backend in BACKENDS {
        let stmt = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>("scoped_items", |q| q)
            .filter(Condition::all().add(node::Column::Id.eq(target)))
            .build_statement(backend);
        let sql = stmt.to_string();

        // One predicate in the CTE body, one in the outer query -- the caller's
        // filter must not have displaced either.
        assert_eq!(
            scope_predicates(&sql),
            2,
            "{backend:?} outer filter displaced a scope predicate: {sql}"
        );
        assert!(
            sql.contains(&target.to_string()),
            "{backend:?} lost the caller's outer filter: {sql}"
        );
    }
}

// ---------------------------------------------------------------------------
// Recursive CTEs
// ---------------------------------------------------------------------------

fn recursive_sql(backend: DbBackend, max_depth: u32) -> String {
    let scope = tenant_scope(uuid::Uuid::new_v4());
    let root = uuid::Uuid::new_v4();
    scoped_nodes(&scope)
        .with_ctes()
        .recursive_cte(RecursiveCte::<node::Entity>::new(
            "subtree",
            Condition::all().add(node::Column::Id.eq(root)),
            node::Column::ParentId,
            node::Column::Id,
            max_depth,
        ))
        .build_statement(backend)
        .to_string()
}

#[test]
fn recursive_cte_emits_with_recursive_on_every_backend() {
    for backend in BACKENDS {
        let sql = recursive_sql(backend, 10);
        assert!(
            sql.trim_start().starts_with("WITH RECURSIVE "),
            "{backend:?}: {sql}"
        );
    }
}

#[test]
fn recursive_cte_scopes_both_seed_and_recursive_member() {
    // The whole reason recursion is admissible under ADR-0001: the recursive
    // member reads the real table, so the scope predicate applies there too. A
    // scope on the seed alone would let the walk escape the tenant after one hop.
    //
    for backend in BACKENDS {
        let sql = recursive_sql(backend, 10);
        let body = cte_body(&sql);
        let (seed, step) = body
            .split_once("UNION")
            .unwrap_or_else(|| panic!("{backend:?} has no recursive member: {sql}"));

        assert_eq!(
            scope_predicates(seed),
            1,
            "{backend:?} seed should carry exactly one tenant predicate: {sql}"
        );
        assert_eq!(
            scope_predicates(step),
            1,
            "{backend:?} recursive member is unscoped -- the walk can leave the tenant: {sql}"
        );
    }
}

#[test]
fn recursive_cte_emits_the_mandatory_depth_cap() {
    // sea_query renders PostgreSQL's CYCLE clause but drops it on MySQL/SQLite,
    // so the depth predicate is the only portable termination guarantee. It is
    // emitted by the library, never optional.
    for backend in BACKENDS {
        let sql = recursive_sql(backend, 7);
        let (_, step) = cte_body(&sql)
            .split_once("UNION")
            .expect("recursive member");
        assert!(
            step.contains("__cte_depth"),
            "{backend:?} recursive member has no depth guard: {sql}"
        );
    }
}

#[test]
fn recursive_cte_depth_cap_reflects_the_requested_bound() {
    // Distinguishes "a cap is emitted" from "the caller's cap is emitted".
    //
    // Comparing two rendered statements would NOT do this: `recursive_sql` mints a
    // fresh tenant and root UUID per call and `to_string()` inlines them, so the
    // strings differ no matter what `max_depth` does. Assert the bound itself.
    for backend in BACKENDS {
        for cap in [0_u32, 3, 90] {
            let sql = recursive_sql(backend, cap);
            let (_, step) = cte_body(&sql)
                .split_once("UNION")
                .expect("recursive member");
            let predicate = format!("__cte_depth\" < {cap}");
            let predicate_mysql = format!("__cte_depth` < {cap}");
            assert!(
                step.contains(&predicate) || step.contains(&predicate_mysql),
                "{backend:?} did not emit the requested bound {cap}: {step}"
            );
        }
    }
}

#[test]
fn recursive_cte_seed_starts_at_depth_zero() {
    for backend in BACKENDS {
        let sql = recursive_sql(backend, 10);
        let (seed, _) = cte_body(&sql)
            .split_once("UNION")
            .expect("recursive member");
        assert!(
            seed.contains("__cte_depth"),
            "{backend:?} seed does not establish the depth column: {sql}"
        );
    }
}

#[test]
fn filter_and_limit_applied_before_with_ctes_survive_into_the_statement() {
    // `with_ctes()` converts the `Select<E>` into a `sea_query::SelectStatement`
    // via `into_query()`. Anything accumulated on the select beforehand -- extra
    // filters, limits -- has to come through that conversion; silently dropping a
    // caller's LIMIT would widen the result set.
    let tenant = uuid::Uuid::new_v4();
    let scope = tenant_scope(tenant);

    for backend in BACKENDS {
        let sql = node::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(node::Column::Name.eq("pre".to_owned())))
            .limit(7)
            .with_ctes()
            .cte::<item::Entity>("scoped_items", |q| q)
            .build_statement(backend)
            .to_string();

        assert!(
            sql.contains("'pre'"),
            "{backend:?} dropped a filter applied before with_ctes(): {sql}"
        );
        assert!(
            sql.contains("LIMIT 7"),
            "{backend:?} dropped a limit applied before with_ctes(): {sql}"
        );
        // And the scope predicate is still there alongside it.
        assert_eq!(
            scope_predicates(&sql),
            2,
            "{backend:?} pre-conversion state displaced a scope predicate: {sql}"
        );
    }
}

#[test]
fn join_cte_accepts_a_compound_on_condition() {
    // A CTE reference often needs more than an equality on the key -- e.g. a BFS
    // hop also constrains the CTE's source column to the current frontier. Pins
    // that `join_cte` takes a full `Condition`, not just a key match.
    let scope = tenant_scope(uuid::Uuid::new_v4());
    let frontier = uuid::Uuid::new_v4();

    let sql = scoped_nodes(&scope)
        .with_ctes()
        .cte::<item::Entity>("scoped_items", |q| q)
        .join_cte(
            "scoped_items",
            Condition::all()
                .add(
                    Expr::col((Alias::new("scoped_items"), Alias::new("node_id")))
                        .equals((node::Entity, node::Column::Id)),
                )
                .add(Expr::col((Alias::new("scoped_items"), Alias::new("id"))).eq(frontier)),
        )
        .build_statement(DbBackend::Postgres)
        .to_string();

    assert!(sql.contains("INNER JOIN \"scoped_items\" ON"), "{sql}");
    assert!(
        sql.contains(&frontier.to_string()),
        "second ON predicate lost: {sql}"
    );
}

#[test]
fn distinct_is_emitted_on_the_outer_query() {
    // `join_cte` is an inner join, so an outer row repeats per matching CTE row.
    // `distinct()` is how a caller collapses that -- e.g. a BFS frontier where
    // several edges land on the same node.
    let scope = tenant_scope(uuid::Uuid::new_v4());

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>("scoped_items", |q| q)
            .join_cte("scoped_items", join_on_node_id())
            .distinct()
            .build_statement(backend)
            .to_string();

        assert!(
            sql.contains("SELECT DISTINCT"),
            "{backend:?} did not emit DISTINCT: {sql}"
        );
        // Only the outer query is affected; the CTE body keeps its own shape.
        assert_eq!(
            sql.matches("SELECT DISTINCT").count(),
            1,
            "{backend:?} DISTINCT leaked into the CTE body: {sql}"
        );
    }
}

fn subtree_cte(root: uuid::Uuid) -> RecursiveCte<node::Entity> {
    RecursiveCte::new(
        "subtree",
        Condition::all().add(node::Column::Id.eq(root)),
        node::Column::ParentId,
        node::Column::Id,
        5,
    )
}

#[test]
fn order_by_cte_targets_the_cte_depth_column() {
    // Ordering a recursive walk shallowest-first needs the CTE's own depth
    // column, which is why `DEPTH_COLUMN` is public.
    let scope = tenant_scope(uuid::Uuid::new_v4());
    let root = uuid::Uuid::new_v4();

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .recursive_cte(subtree_cte(root))
            .join_cte("subtree", join_on_subtree_id())
            .order_by_cte(
                "subtree",
                RecursiveCte::<node::Entity>::DEPTH_COLUMN,
                Order::Asc,
            )
            .build_statement(backend)
            .to_string();

        let (_, tail) = sql.split_once("ORDER BY").unwrap_or_else(|| {
            panic!("{backend:?} did not emit ORDER BY: {sql}");
        });
        assert!(
            tail.contains("__cte_depth"),
            "{backend:?} ordered by the wrong column: {sql}"
        );
    }
}

#[test]
fn order_by_qualifies_the_entity_column_with_its_table() {
    // Unqualified `ORDER BY "id"` would be ambiguous after joining a CTE that
    // also exposes `id` -- which the recursive CTE does, since it selects all of
    // the entity's columns.
    let scope = tenant_scope(uuid::Uuid::new_v4());
    let root = uuid::Uuid::new_v4();

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .recursive_cte(subtree_cte(root))
            .join_cte("subtree", join_on_subtree_id())
            .order_by(node::Column::Id, Order::Desc)
            .build_statement(backend)
            .to_string();

        let (_, tail) = sql.split_once("ORDER BY").expect("ORDER BY");
        let qualified = tail.contains("\"cte_node\".\"id\"") || tail.contains("`cte_node`.`id`");
        assert!(
            qualified,
            "{backend:?} emitted an unqualified ORDER BY column: {sql}"
        );
    }
}

#[test]
fn distinct_with_order_by_entity_column_is_allowed() {
    // The valid half of the pairing: an entity column IS in the outer SELECT
    // list, so DISTINCT + ORDER BY on it is portable. A blanket rejection of
    // distinct-plus-any-order-by would have wrongly blocked this.
    let scope = tenant_scope(uuid::Uuid::new_v4());

    let query = scoped_nodes(&scope)
        .with_ctes()
        .cte::<item::Entity>("scoped_items", |q| q)
        .join_cte("scoped_items", join_on_node_id())
        .distinct()
        .order_by(node::Column::Id, Order::Asc);

    assert!(
        query.validate().is_ok(),
        "distinct + entity-column ordering must be permitted"
    );
}

#[test]
fn distinct_with_order_by_cte_is_rejected() {
    // PostgreSQL and MySQL both refuse ORDER BY on an expression absent from a
    // SELECT DISTINCT list; SQLite accepts it. Rejecting in the library makes the
    // failure identical everywhere instead of surfacing only in production.
    let scope = tenant_scope(uuid::Uuid::new_v4());
    let root = uuid::Uuid::new_v4();

    let query = scoped_nodes(&scope)
        .with_ctes()
        .recursive_cte(subtree_cte(root))
        .join_cte("subtree", join_on_subtree_id())
        .distinct()
        .order_by_cte(
            "subtree",
            RecursiveCte::<node::Entity>::DEPTH_COLUMN,
            Order::Asc,
        );

    let Err(err) = query.validate() else {
        panic!("distinct + order_by_cte must be rejected")
    };
    assert!(
        matches!(err, ScopeError::Invalid(msg) if msg.contains("DISTINCT")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn order_by_cte_alone_is_allowed() {
    // Ordering by a CTE column is only a problem under DISTINCT. On its own it is
    // valid on all three backends, so it must not be rejected.
    let scope = tenant_scope(uuid::Uuid::new_v4());
    let root = uuid::Uuid::new_v4();

    let query = scoped_nodes(&scope)
        .with_ctes()
        .recursive_cte(subtree_cte(root))
        .join_cte("subtree", join_on_subtree_id())
        .order_by_cte(
            "subtree",
            RecursiveCte::<node::Entity>::DEPTH_COLUMN,
            Order::Asc,
        );

    assert!(query.validate().is_ok(), "ordering alone must be permitted");
}

/// `ON scoped_items.node_id = cte_node.id`
fn join_on_node_id() -> Condition {
    Condition::all().add(
        Expr::col((Alias::new("scoped_items"), Alias::new("node_id")))
            .equals((node::Entity, node::Column::Id)),
    )
}

/// `ON subtree.id = cte_node.id`
fn join_on_subtree_id() -> Condition {
    Condition::all().add(
        Expr::col((Alias::new("subtree"), Alias::new("id")))
            .equals((node::Entity, node::Column::Id)),
    )
}

// ---------------------------------------------------------------------------
// Projection on the outer query
// ---------------------------------------------------------------------------

#[test]
fn select_only_narrows_the_outer_select_list() {
    // Raised in review: the outer query inherits `Select<E>`'s projection, which is
    // every column of E, so a hop needing only ids still transferred the wide
    // columns -- an index-only scan degraded into a heap visit per row.
    let scope = tenant_scope(uuid::Uuid::new_v4());

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>("x", |q| q)
            .join_cte("x", join_on_node_id())
            .select_only()
            .column(node::Column::Id)
            .build_statement(backend)
            .to_string();

        // Mandatory, not a fallback: defaulting to the whole statement would make
        // the assertion below inspect the CTE body too, degrading the test into a
        // pass instead of failing when the rendering changes.
        let (head, _) = sql
            .split_once(" FROM \"cte_node\"")
            .or_else(|| sql.split_once(" FROM `cte_node`"))
            .unwrap_or_else(|| panic!("{backend:?} no outer FROM clause: {sql}"));
        // The outer SELECT must carry exactly the one requested column. `name` is
        // the marker: it is in E but must not survive select_only().
        assert!(
            !head.contains("\"name\"") && !head.contains("`name`"),
            "{backend:?} outer SELECT still carries every column: {sql}"
        );
        assert!(
            head.contains("\"id\"") || head.contains("`id`"),
            "{backend:?} requested column missing: {sql}"
        );
    }
}

#[test]
fn select_only_does_not_disturb_the_scope_predicate() {
    // Projection and scope are independent; narrowing the select list must not
    // drop the tenant filter from either the body or the outer query.
    let scope = tenant_scope(uuid::Uuid::new_v4());

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>("x", |q| q)
            .join_cte("x", join_on_node_id())
            .select_only()
            .column(node::Column::Id)
            .build_statement(backend)
            .to_string();
        assert_eq!(
            scope_predicates(&sql),
            2,
            "{backend:?} select_only() disturbed a scope predicate: {sql}"
        );
    }
}

#[test]
fn column_from_cte_and_expr_as_extend_the_outer_select_list() {
    // A CTE column needs an alias to avoid colliding with the entity's own column
    // of the same name; an expression covers aggregates and computed values.
    let scope = tenant_scope(uuid::Uuid::new_v4());
    let root = uuid::Uuid::new_v4();

    let sql = scoped_nodes(&scope)
        .with_ctes()
        .recursive_cte(subtree_cte(root))
        .join_cte("subtree", join_on_subtree_id())
        .select_only()
        .column(node::Column::Id)
        .column_from_cte("subtree", RecursiveCte::<node::Entity>::DEPTH_COLUMN, "hop")
        .expr_as(Expr::val(1).add(Expr::val(1)), "two")
        .build_statement(DbBackend::Postgres)
        .to_string();

    assert!(sql.contains(r#"AS "hop""#), "CTE column not aliased: {sql}");
    assert!(sql.contains(r#"AS "two""#), "expression not aliased: {sql}");
}

// ---------------------------------------------------------------------------
// Recursive dedup mode
// ---------------------------------------------------------------------------

#[test]
fn recursive_cte_defaults_to_union_not_union_all() {
    // Raised in review with measurements: UNION ALL enumerates paths, so on a graph
    // where equal-length paths converge the row count multiplies with the branching
    // factor. Plain UNION prunes that and is standard SQL on all three backends, so
    // it is the safer default.
    let scope = tenant_scope(uuid::Uuid::new_v4());
    let root = uuid::Uuid::new_v4();

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .recursive_cte(subtree_cte(root))
            .build_statement(backend)
            .to_string();
        let body = cte_body(&sql);
        assert!(
            body.contains("UNION"),
            "{backend:?} no UNION in the recursive body: {sql}"
        );
        assert!(
            !body.contains("UNION ALL"),
            "{backend:?} defaulted to UNION ALL, which enumerates paths: {sql}"
        );
    }
}

#[test]
fn recursive_cte_dedup_union_all_is_opt_in() {
    let scope = tenant_scope(uuid::Uuid::new_v4());
    let root = uuid::Uuid::new_v4();

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .recursive_cte(subtree_cte(root).dedup(RecursiveDedup::UnionAll))
            .build_statement(backend)
            .to_string();
        assert!(
            cte_body(&sql).contains("UNION ALL"),
            "{backend:?} opt-in UNION ALL not emitted: {sql}"
        );
    }
}

// ---------------------------------------------------------------------------
// Aggregation inside the CTE body
// ---------------------------------------------------------------------------

#[test]
fn cte_body_supports_aggregation_with_scope_intact() {
    // Answers a design question raised on the tracking issue: degree-ordered
    // truncation needs an aggregate computed *inside* the scoped CTE body. The
    // closure receives a `Select<J>`, so the whole `QuerySelect` surface is
    // available -- and, critically, the scope predicate survives `select_only()`,
    // which is what makes the aggregate safe to compute server-side.
    use sea_orm::QuerySelect;
    let scope = tenant_scope(uuid::Uuid::new_v4());

    for backend in BACKENDS {
        let sql = scoped_nodes(&scope)
            .with_ctes()
            .cte::<item::Entity>("degree", |q| {
                q.select_only()
                    .column(item::Column::NodeId)
                    .column_as(Expr::col(item::Column::Id).count(), "degree")
                    .group_by(item::Column::NodeId)
            })
            .build_statement(backend)
            .to_string();

        let body = cte_body(&sql);
        assert!(body.contains("COUNT"), "{backend:?} no aggregate: {sql}");
        assert!(body.contains("GROUP BY"), "{backend:?} no GROUP BY: {sql}");
        assert_eq!(
            scope_predicates(body),
            1,
            "{backend:?} select_only() in the body dropped its scope: {sql}"
        );
    }
}
