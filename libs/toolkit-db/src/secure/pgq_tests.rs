//! Tests for the property-graph declaration.
//!
//! What is asserted here is not syntax — the syntax layer has its own tests —
//! but the three guarantees the declaration exists to provide: scope columns
//! reach `PROPERTIES` without the caller repeating them, an element that cannot
//! be scoped is refused, and a label cannot be shared.

use super::pgq::{Endpoint, GraphDeclaration, PropertyGraph, VertexOf};
use crate::secure::{ScopableEntity, ScopeError};

/// A vertex with the usual tenant/resource dimensions.
mod node {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "graph_node")]
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
            use toolkit_security::access_scope::pep_properties;
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

/// An edge that carries a tenant, as a secure graph element must.
mod edge {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "graph_edge")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub tenant_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        pub src_node_id: i64,
        pub dst_node_id: i64,
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
            use toolkit_security::access_scope::pep_properties;
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

/// A vertex whose scope columns are **not** all key columns. Without one of
/// these, a test asserting "the scope columns reached PROPERTIES" passes even if
/// the list was built from the key alone, because the two sets coincide.
mod owned {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "owned_thing")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i64,
        pub tenant_id: Uuid,
        pub owner_id: Uuid,
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
            Some(Column::OwnerId)
        }
        fn type_col() -> Option<Column> {
            None
        }
        fn resolve_property(property: &str) -> Option<Column> {
            use toolkit_security::access_scope::pep_properties;
            match property {
                p if p == pep_properties::OWNER_TENANT_ID => Some(Column::TenantId),
                p if p == pep_properties::RESOURCE_ID => Some(Column::Id),
                p if p == pep_properties::OWNER_ID => Some(Column::OwnerId),
                _ => None,
            }
        }
        fn scope_columns() -> Vec<Column> {
            vec![Column::TenantId, Column::Id, Column::OwnerId]
        }
    }
}

/// A closure table, shaped exactly like the platform's real ones: a pair of
/// foreign keys and no scope dimension of its own. Correct for that table and
/// consequently ineligible as a graph element.
mod closure {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "tenant_closure")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub ancestor_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub descendant_id: Uuid,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    impl crate::secure::ScopableEntity for Entity {
        fn tenant_col() -> Option<Column> {
            None
        }
        fn resource_col() -> Option<Column> {
            None
        }
        fn owner_col() -> Option<Column> {
            None
        }
        fn type_col() -> Option<Column> {
            None
        }
        fn resolve_property(_property: &str) -> Option<Column> {
            None
        }
        fn scope_columns() -> Vec<Column> {
            Vec::new()
        }
    }
}

struct Kb;

impl PropertyGraph for Kb {
    const GRAPH_NAME: &'static str = "kb_pgq";

    fn declaration() -> Result<GraphDeclaration, ScopeError> {
        GraphDeclaration::new::<Self>()
            .vertex::<Self, node::Entity>(&["tenant_id", "id"])?
            .edge::<Self, edge::Entity>(
                &["tenant_id", "id"],
                Endpoint {
                    key: vec!["tenant_id".to_owned(), "src_node_id".to_owned()],
                    table: "graph_node".to_owned(),
                    references: vec!["tenant_id".to_owned(), "id".to_owned()],
                },
                Endpoint {
                    key: vec!["tenant_id".to_owned(), "dst_node_id".to_owned()],
                    table: "graph_node".to_owned(),
                    references: vec!["tenant_id".to_owned(), "id".to_owned()],
                },
            )
    }
}

impl VertexOf<Kb> for node::Entity {
    const LABEL: &'static str = "node";
}

impl super::pgq::EdgeOf<Kb> for edge::Entity {
    const LABEL: &'static str = "edge";
}

/// The caller listed only the key columns. The scope columns arrive because the
/// declaration reads them off the entity — which is the whole point: a scope
/// column absent from `PROPERTIES` is invisible to `MATCH`, silently.
#[test]
fn scope_columns_reach_the_properties_list_without_being_repeated() {
    let declaration = Kb::declaration().expect("declares");
    let node_props = declaration.properties_of("node").expect("node is declared");
    assert!(node_props.contains(&"tenant_id".to_owned()));
    assert!(node_props.contains(&"id".to_owned()));
    // And nothing that is not a key or a scope column: a wider list is not
    // wrong, but it would mean the source of the list is not the entity.
    assert_eq!(node_props.len(), 2, "{node_props:?}");
}

/// The distinguishing case: a scope column that is not a key column must still
/// reach `PROPERTIES`. Asserted separately because in an entity whose key *is*
/// its scope columns, a list built from the key alone looks identical — a test
/// that only covers that shape passes against a declaration that ignores the
/// entity entirely.
#[test]
fn a_scope_column_outside_the_key_still_reaches_the_properties_list() {
    struct Owned;
    impl PropertyGraph for Owned {
        const GRAPH_NAME: &'static str = "owned";
        fn declaration() -> Result<GraphDeclaration, ScopeError> {
            // Keyed on `id` only; `tenant_id` and `owner_id` are scope columns
            // that appear nowhere in the key.
            GraphDeclaration::new::<Self>().vertex::<Self, owned::Entity>(&["id"])
        }
    }
    impl VertexOf<Owned> for owned::Entity {
        const LABEL: &'static str = "owned";
    }

    let declaration = Owned::declaration().expect("declares");
    let props = declaration.properties_of("owned").expect("declared");
    assert!(props.contains(&"id".to_owned()), "{props:?}");
    assert!(
        props.contains(&"tenant_id".to_owned()),
        "the tenant column must be filterable inside a pattern: {props:?}"
    );
    assert!(
        props.contains(&"owner_id".to_owned()),
        "the owner column must be filterable inside a pattern: {props:?}"
    );
}

/// The edge is scoped too. An edge table that carried no tenant would be
/// ineligible, which is the modelling constraint Policy 2 puts on gears.
#[test]
fn the_edge_exposes_its_scope_columns_as_well() {
    let declaration = Kb::declaration().expect("declares");
    let props = declaration.properties_of("edge").expect("edge is declared");
    assert!(props.contains(&"tenant_id".to_owned()), "{props:?}");
}

/// The failure this refusal prevents is silent: such an element compiles to
/// `WHERE false` under any constrained scope, so the traversal returns nothing
/// and looks like missing data.
#[test]
fn an_element_that_resolves_no_scope_column_is_refused() {
    struct Bad;
    impl PropertyGraph for Bad {
        const GRAPH_NAME: &'static str = "bad";
        fn declaration() -> Result<GraphDeclaration, ScopeError> {
            GraphDeclaration::new::<Self>().vertex::<Self, closure::Entity>(&["ancestor_id"])
        }
    }
    impl VertexOf<Bad> for closure::Entity {
        const LABEL: &'static str = "closure";
    }

    let err = Bad::declaration().expect_err("a closure table cannot be a graph element");
    assert!(
        matches!(err, ScopeError::Invalid(msg) if msg.contains("at least one scope column")),
        "unexpected error: {err}"
    );
}

/// Sharing a label across element tables is legal SQL/PGQ and unsafe here:
/// security is decided per entity, so one label would carry several mappings.
#[test]
fn two_elements_cannot_share_a_label() {
    struct Shared;
    impl PropertyGraph for Shared {
        const GRAPH_NAME: &'static str = "shared";
        fn declaration() -> Result<GraphDeclaration, ScopeError> {
            GraphDeclaration::new::<Self>()
                .vertex::<Self, node::Entity>(&["tenant_id", "id"])?
                .vertex::<Self, edge::Entity>(&["tenant_id", "id"])
        }
    }
    impl VertexOf<Shared> for node::Entity {
        const LABEL: &'static str = "thing";
    }
    impl VertexOf<Shared> for edge::Entity {
        const LABEL: &'static str = "thing";
    }

    let err = Shared::declaration().expect_err("a shared label must be refused");
    assert!(
        matches!(err, ScopeError::Invalid(msg) if msg.contains("share a label")),
        "unexpected error: {err}"
    );
}

/// An element with no key cannot be referenced by an edge, so it is refused
/// rather than declared and discovered later.
#[test]
fn an_element_without_a_key_is_refused() {
    struct NoKey;
    impl PropertyGraph for NoKey {
        const GRAPH_NAME: &'static str = "nokey";
        fn declaration() -> Result<GraphDeclaration, ScopeError> {
            GraphDeclaration::new::<Self>().vertex::<Self, node::Entity>(&[])
        }
    }
    impl VertexOf<NoKey> for node::Entity {
        const LABEL: &'static str = "node";
    }
    assert!(NoKey::declaration().is_err());
}

/// The composite endpoint key is what makes an edge structurally unable to join
/// a vertex of another tenant, before any scope predicate is applied — so the
/// DDL must actually carry it.
#[test]
fn the_rendered_ddl_carries_composite_endpoint_keys() {
    let sql = Kb::declaration()
        .expect("declares")
        .create_statement()
        .expect("renders");
    assert!(sql.contains(r#"CREATE PROPERTY GRAPH "kb_pgq""#), "{sql}");
    assert!(
        sql.contains(
            r#"SOURCE KEY ("tenant_id", "src_node_id") REFERENCES "graph_node" ("tenant_id", "id")"#
        ),
        "{sql}"
    );
    assert!(
        sql.contains(r#"LABEL "node" PROPERTIES ("tenant_id", "id")"#),
        "{sql}"
    );
}

#[test]
fn the_drop_statement_matches_the_declared_name() {
    let sql = Kb::declaration()
        .expect("declares")
        .drop_statement()
        .expect("renders");
    assert_eq!(sql, r#"DROP PROPERTY GRAPH IF EXISTS "kb_pgq""#);
}

/// An endpoint that carries two columns and references one would produce a DDL
/// `PostgreSQL` rejects, so it is caught here where the message can be useful.
#[test]
fn a_mismatched_endpoint_arity_is_refused() {
    struct Mismatched;
    impl PropertyGraph for Mismatched {
        const GRAPH_NAME: &'static str = "mismatched";
        fn declaration() -> Result<GraphDeclaration, ScopeError> {
            GraphDeclaration::new::<Self>()
                .vertex::<Self, node::Entity>(&["tenant_id", "id"])?
                .edge::<Self, edge::Entity>(
                    &["tenant_id", "id"],
                    Endpoint {
                        key: vec!["tenant_id".to_owned(), "src_node_id".to_owned()],
                        table: "graph_node".to_owned(),
                        references: vec!["id".to_owned()],
                    },
                    Endpoint {
                        key: vec!["tenant_id".to_owned(), "dst_node_id".to_owned()],
                        table: "graph_node".to_owned(),
                        references: vec!["tenant_id".to_owned(), "id".to_owned()],
                    },
                )
        }
    }
    impl VertexOf<Mismatched> for node::Entity {
        const LABEL: &'static str = "node";
    }
    impl super::pgq::EdgeOf<Mismatched> for edge::Entity {
        const LABEL: &'static str = "edge";
    }

    let err = Mismatched::declaration().expect_err("arity must match");
    assert!(
        matches!(err, ScopeError::GraphSyntax(ref msg) if msg.contains("as many columns")),
        "unexpected error: {err}"
    );
}

/// `scope_columns` has to include `pep_prop` entries, because `resolve_property`
/// can resolve them and therefore a scope can address them - so a pattern must
/// be able to filter on them. Only the derive knows that set; the trait's
/// default cannot.
#[test]
fn the_derive_enumerates_custom_pep_properties() {
    mod derived {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, toolkit_db_macros::Scopable)]
        #[sea_orm(table_name = "with_custom")]
        #[secure(
            tenant_col = "tenant_id",
            resource_col = "id",
            no_owner,
            no_type,
            pep_prop(department_id = "department_id")
        )]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: Uuid,
            pub tenant_id: Uuid,
            pub department_id: Uuid,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    let names: Vec<&str> = <derived::Entity as ScopableEntity>::scope_columns()
        .iter()
        .map(sea_orm::IdenStatic::as_str)
        .collect();
    assert!(names.contains(&"tenant_id"), "{names:?}");
    assert!(names.contains(&"id"), "{names:?}");
    assert!(
        names.contains(&"department_id"),
        "a pep_prop column is a scope column: {names:?}"
    );
}

// ─────────────────── the secure graph query: rendered SQL ───────────────────

use crate::secure::SecureEntityExt as _;
use sea_orm::EntityTrait as _;
use sea_orm::sea_query::ExprTrait as _;
use toolkit_security::AccessScope;

/// Each element's own body, keyed by its variable.
///
/// Counting occurrences of a scope column across the whole statement is the trap
/// ADR-0001 records: the column also appears in `COLUMNS` and in the outer
/// `WHERE`, so a count passes even when one element body carries no predicate at
/// all. Every assertion below is therefore per element.
fn element_bodies(sql: &str) -> std::collections::BTreeMap<String, String> {
    let mut bodies = std::collections::BTreeMap::new();
    // Only the MATCH region holds elements. Scanning the whole statement would
    // also pick up the GRAPH_TABLE wrapper and the COLUMNS list, both of which
    // open with a quoted name.
    let Some(after_match) = sql.split(" MATCH ").nth(1) else {
        return bodies;
    };
    let pattern = after_match
        .split(" COLUMNS (")
        .next()
        .unwrap_or(after_match);
    let bytes: Vec<char> = pattern.chars().collect();
    let mut index = 0;
    while index < bytes.len() {
        let open = bytes[index];
        let close = match open {
            '(' => ')',
            '[' => ']',
            _ => {
                index += 1;
                continue;
            }
        };
        // An element opens with a quoted variable immediately after the bracket.
        if bytes.get(index + 1) != Some(&'"') {
            index += 1;
            continue;
        }
        let mut depth = 1;
        let mut cursor = index + 1;
        while cursor < bytes.len() && depth > 0 {
            if bytes[cursor] == open {
                depth += 1;
            } else if bytes[cursor] == close {
                depth -= 1;
            }
            cursor += 1;
        }
        let body: String = bytes[index + 1..cursor.saturating_sub(1)].iter().collect();
        if let Some(variable) = body
            .strip_prefix('"')
            .and_then(|rest| rest.split('"').next())
            && body.contains(" IS ")
        {
            bodies.insert(variable.to_owned(), body.clone());
        }
        index += 1;
    }
    bodies
}

fn tenant_scope() -> AccessScope {
    AccessScope::for_tenant(uuid::Uuid::from_u128(0x5150))
}

fn subtree_scope() -> AccessScope {
    AccessScope::from_constraints(vec![toolkit_security::access_scope::ScopeConstraint::new(
        vec![
            toolkit_security::access_scope::ScopeFilter::in_tenant_subtree(
                toolkit_security::access_scope::pep_properties::OWNER_TENANT_ID,
                toolkit_security::ScopeValue::Uuid(uuid::Uuid::from_u128(0x5150)),
                true,
                vec![],
            ),
        ],
    )])
}

/// A two-hop query under `scope`, rendered.
fn two_hop(scope: &AccessScope) -> Result<String, ScopeError> {
    node::Entity::find()
        .secure()
        .scope_with(scope)
        .with_graph::<Kb>()
        .match_path(|p| {
            p.vertex::<node::Entity>("a")
                .edge_to::<edge::Entity>("e")
                .to::<node::Entity>("b")
        })
        .column("b", "id", "neighbour")
        .build_statement(sea_orm::DbBackend::Postgres)
        .map(|stmt| stmt.sql)
}

/// The central guarantee: the scope predicate is inside **each** element body,
/// vertices and edges alike, qualified by that element's own variable.
#[test]
fn every_graph_element_embeds_scope() {
    let sql = two_hop(&tenant_scope()).expect("builds");
    let bodies = element_bodies(&sql);
    assert_eq!(bodies.len(), 3, "expected three elements: {bodies:?}");
    for (variable, body) in &bodies {
        assert!(
            body.contains(&format!(r#""{variable}"."tenant_id""#)),
            "element `{variable}` carries no scope predicate of its own: {body}"
        );
    }
}

/// A pattern with no caller predicate has no way to reference the anchor, so
/// the anchor is dropped from the `FROM` rather than left as an uncorrelated
/// comma join — which would multiply every match by the anchor's row count.
#[test]
fn an_uncorrelated_anchor_is_dropped_from_the_from_clause() {
    let sql = two_hop(&tenant_scope()).expect("builds");
    assert!(
        !sql.contains(r#""graph_node""#),
        "an anchor nothing references must not be in the FROM: {sql}"
    );
    // Isolation is not lost with it: every element still carries the scope.
    for (variable, body) in element_bodies(&sql) {
        assert!(
            body.contains(&format!(r#""{variable}"."tenant_id""#)),
            "{body}"
        );
    }
}

/// The "start from these rows, walk out one hop" shape: a caller predicate
/// correlating an element with the anchor keeps the anchor in the `FROM` —
/// scoped by its own outer `WHERE` — and that outer `WHERE` is still not a
/// substitute for element scope, which every body carries regardless.
#[test]
fn the_outer_where_does_not_stand_in_for_element_scope() {
    use sea_orm::sea_query::{Alias, Expr};

    let sql = node::Entity::find()
        .secure()
        .scope_with(&tenant_scope())
        .with_graph::<Kb>()
        .match_path(|p| {
            p.vertex::<node::Entity>("a")
                .where_(
                    sea_orm::Condition::all().add(
                        Expr::col((Alias::new("a"), Alias::new("id")))
                            .eq(Expr::col((Alias::new("graph_node"), Alias::new("id")))),
                    ),
                )
                .correlate_with_anchor()
                .edge_to::<edge::Entity>("e")
                .to::<node::Entity>("b")
        })
        .column("b", "id", "neighbour")
        .build_statement(sea_orm::DbBackend::Postgres)
        .expect("builds")
        .sql;

    let (pattern, outer) = sql.split_once(" AS \"cf_graph\"").expect("aliased");
    assert!(
        pattern.contains(r#"FROM "graph_node""#),
        "a correlated anchor stays in the FROM: {pattern}"
    );
    assert!(
        outer.contains(r#""graph_node"."tenant_id""#),
        "the anchor must still be scoped: {outer}"
    );
    // And removing the outer clause from consideration leaves the pattern fully
    // scoped on its own.
    for (variable, body) in element_bodies(pattern) {
        assert!(
            body.contains(&format!(r#""{variable}"."tenant_id""#)),
            "{body}"
        );
    }
}

/// A caller predicate narrows an element; it cannot remove what the library put
/// there. Both must survive in the same element body, the caller's predicate
/// first and the scope AND-ed after it — scope is applied on top, so a
/// predicate cannot filter it back off.
#[test]
fn a_caller_predicate_cannot_remove_element_scope() {
    use sea_orm::sea_query::{Alias, Expr};

    // A predicate that would exclude every scoped row, were it able to win.
    let excluding = sea_orm::Condition::all().add(
        Expr::col((Alias::new("a"), Alias::new("tenant_id"))).eq(uuid::Uuid::from_u128(0xDEAD)),
    );
    let sql = node::Entity::find()
        .secure()
        .scope_with(&tenant_scope())
        .with_graph::<Kb>()
        .match_path(|p| {
            p.vertex::<node::Entity>("a")
                .where_(excluding)
                .edge_to::<edge::Entity>("e")
                .to::<node::Entity>("b")
        })
        .column("b", "id", "neighbour")
        .build_statement(sea_orm::DbBackend::Postgres)
        .expect("builds")
        .sql;

    let bodies = element_bodies(&sql);
    let body = &bodies["a"];
    let caller = body.find(" = ").expect("the caller predicate renders");
    let occurrences = body.matches(r#""a"."tenant_id""#).count();
    assert_eq!(
        occurrences, 2,
        "both the caller predicate and the scope must survive in one body: {body}"
    );
    let scope = body
        .rfind(r#""a"."tenant_id" IN"#)
        .expect("the scope renders");
    assert!(
        caller < scope,
        "the caller predicate must precede the scope, so scope is AND-ed on top: {body}"
    );
    assert!(body.contains(" AND "), "{body}");
}

/// Deny-all reaches every element rather than only the outer query.
///
/// The `false` is a bound parameter, not the literal `FALSE`, so the assertion
/// is on the element having a predicate at all plus the statement binding
/// `false` — checking the text for "FALSE" would fail on correct output.
#[test]
fn deny_all_reaches_every_element() {
    let stmt = node::Entity::find()
        .secure()
        .scope_with(&AccessScope::deny_all())
        .with_graph::<Kb>()
        .match_path(|p| {
            p.vertex::<node::Entity>("a")
                .edge_to::<edge::Entity>("e")
                .to::<node::Entity>("b")
        })
        .column("b", "id", "neighbour")
        .build_statement(sea_orm::DbBackend::Postgres)
        .expect("builds");

    let bodies = element_bodies(&stmt.sql);
    assert_eq!(bodies.len(), 3);
    for (variable, body) in &bodies {
        assert!(
            body.contains(" WHERE "),
            "element `{variable}` was left unrestricted under deny-all: {body}"
        );
    }
    let bound = format!("{:?}", stmt.values.expect("bound values").0);
    assert!(
        bound.contains("Bool(Some(false))"),
        "deny-all must bind a false: {bound}"
    );
}

/// Allow-all must not invent a restriction. An unconstrained scope compiles to
/// an empty condition, which the AST drops rather than rendering as
/// `WHERE TRUE` — so "no predicate" is visibly no predicate.
#[test]
fn allow_all_adds_no_element_predicate() {
    let sql = two_hop(&AccessScope::allow_all()).expect("builds");
    for (variable, body) in element_bodies(&sql) {
        assert!(
            !body.contains(" WHERE "),
            "element `{variable}` acquired a predicate under allow-all: {body}"
        );
    }
}

/// A subtree scope is servable: the closure is placed once as a correlated
/// sibling and every element references it. Placed twice it would become a
/// cross join and multiply rows.
#[test]
fn a_subtree_scope_places_one_correlated_sibling() {
    let sql = two_hop(&subtree_scope()).expect("builds");

    assert_eq!(
        sql.matches(r#"AS "__cf_scope_0_0""#).count(),
        1,
        "the sibling must be placed exactly once: {sql}"
    );
    let bodies = element_bodies(&sql);
    assert_eq!(bodies.len(), 3);
    for (variable, body) in &bodies {
        assert!(
            body.contains(r#""__cf_scope_0_0"."descendant_id""#),
            "element `{variable}` does not correlate against the sibling: {body}"
        );
        assert!(
            !body.contains("SELECT"),
            "no subquery may appear inside a pattern predicate: {body}"
        );
    }
}

/// An element whose entity resolves no scope column is refused at build time —
/// not turned into a traversal that returns nothing and looks like missing data.
#[test]
fn an_unscopable_element_is_refused() {
    struct Loose;
    impl PropertyGraph for Loose {
        const GRAPH_NAME: &'static str = "loose";
        fn declaration() -> Result<GraphDeclaration, ScopeError> {
            GraphDeclaration::new::<Self>().vertex::<Self, node::Entity>(&["tenant_id", "id"])
        }
    }
    impl VertexOf<Loose> for node::Entity {
        const LABEL: &'static str = "node";
    }
    // The closure table is a legitimate table and an illegitimate graph element.
    impl VertexOf<Loose> for closure::Entity {
        const LABEL: &'static str = "closure";
    }

    let err = node::Entity::find()
        .secure()
        .scope_with(&tenant_scope())
        .with_graph::<Loose>()
        .match_path(|p| p.vertex::<closure::Entity>("c"))
        .column("c", "ancestor_id", "x")
        .build_statement(sea_orm::DbBackend::Postgres)
        .expect_err("an unscopable element must be refused");
    assert!(
        matches!(err, ScopeError::Invalid(msg) if msg.contains("at least one scope column")),
        "unexpected error: {err}"
    );
}

/// Pattern-shape mistakes surface as an error at build time. They are the
/// caller's correctness problem rather than an isolation problem, but they must
/// not be silent.
#[test]
fn pattern_shape_mistakes_are_reported() {
    let dangling = node::Entity::find()
        .secure()
        .scope_with(&tenant_scope())
        .with_graph::<Kb>()
        .match_path(|p| p.vertex::<node::Entity>("a").edge_to::<edge::Entity>("e"))
        .column("a", "id", "x")
        .build_statement(sea_orm::DbBackend::Postgres);
    assert!(dangling.is_err(), "an unfinished hop must be refused");

    let no_head = node::Entity::find()
        .secure()
        .scope_with(&tenant_scope())
        .with_graph::<Kb>()
        .match_path(|p| p.edge_to::<edge::Entity>("e"))
        .column("e", "id", "x")
        .build_statement(sea_orm::DbBackend::Postgres);
    assert!(no_head.is_err(), "a pattern must start at a vertex");

    let no_pattern = node::Entity::find()
        .secure()
        .scope_with(&tenant_scope())
        .with_graph::<Kb>()
        .column("a", "id", "x")
        .build_statement(sea_orm::DbBackend::Postgres);
    assert!(no_pattern.is_err(), "a graph query needs a pattern");
}

/// Placeholders are shared only between predicates that bind the same values.
/// Two elements needing different values must not collide — the sharing seen
/// under a single-tenant scope is value deduplication, not a numbering bug.
#[test]
fn elements_binding_different_values_get_different_placeholders() {
    let t1 = uuid::Uuid::from_u128(0x1111);
    let t2 = uuid::Uuid::from_u128(0x2222);
    let scope =
        AccessScope::from_constraints(vec![toolkit_security::access_scope::ScopeConstraint::new(
            vec![toolkit_security::access_scope::ScopeFilter::in_uuids(
                toolkit_security::access_scope::pep_properties::OWNER_TENANT_ID,
                vec![t1, t2],
            )],
        )]);
    let stmt = node::Entity::find()
        .secure()
        .scope_with(&scope)
        .with_graph::<Kb>()
        .match_path(|p| {
            p.vertex::<node::Entity>("a")
                .edge_to::<edge::Entity>("e")
                .to::<node::Entity>("b")
        })
        .column("b", "id", "neighbour")
        .build_statement(sea_orm::DbBackend::Postgres)
        .expect("builds");

    let values = stmt.values.expect("bound values");
    // Every bound value is one of the two tenants, and both appear: a numbering
    // mistake would bind something else or drop one.
    assert!(values.0.len() >= 6, "{:?}", values.0);
    let rendered = format!("{:?}", values.0);
    assert!(
        rendered.contains("1111") && rendered.contains("2222"),
        "{rendered}"
    );
}

/// The exact bound-value list, for a pattern whose elements bind *different*
/// numbers of values. Under a symmetric pattern (every element compiling the
/// same scope) a placeholder-numbering bug is invisible: the wrong index still
/// lands on the right value. Here the edge carries one extra caller value, so
/// any renumbering or dropped binding shifts the list and fails the equality.
#[test]
fn the_exact_value_list_is_bound_when_elements_differ() {
    use sea_orm::sea_query::{Alias, Expr};

    let tenant = uuid::Uuid::from_u128(0x5150);
    let stmt = node::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant))
        .with_graph::<Kb>()
        .match_path(|p| {
            p.vertex::<node::Entity>("a")
                .edge_to::<edge::Entity>("e")
                .where_(
                    sea_orm::Condition::all()
                        .add(Expr::col((Alias::new("e"), Alias::new("id"))).eq(5_i64)),
                )
                .to::<node::Entity>("b")
        })
        .column("b", "id", "neighbour")
        .build_statement(sea_orm::DbBackend::Postgres)
        .expect("builds");

    let values = stmt.values.expect("bound values");
    assert_eq!(
        values.0,
        vec![
            // element `a`: its scope.
            sea_orm::Value::from(tenant),
            // element `e`: the caller predicate first, then its scope.
            sea_orm::Value::from(5_i64),
            sea_orm::Value::from(tenant),
            // element `b`: its scope. No anchor value: without
            // `correlate_with_anchor()` the anchor is not in the statement.
            sea_orm::Value::from(tenant),
        ],
        "values must bind in pattern order, caller predicate before scope"
    );
    assert!(
        stmt.sql.contains("$4") && !stmt.sql.contains("$5"),
        "exactly four placeholders: {}",
        stmt.sql
    );
}

/// A scope made of one empty constraint is neither unconstrained nor deny-all,
/// yet compiles to an empty predicate. Attaching nothing would leave the
/// element traversing every tenant's rows, so it is refused: "no predicate" is
/// reachable only from an explicitly unconstrained scope.
#[test]
fn an_empty_constraint_scope_is_refused_for_elements() {
    let scope =
        AccessScope::from_constraints(vec![toolkit_security::access_scope::ScopeConstraint::new(
            vec![],
        )]);
    assert!(!scope.is_unconstrained() && !scope.is_deny_all());

    let err = two_hop(&scope).expect_err("an empty predicate must be refused");
    assert!(
        matches!(err, ScopeError::Invalid(msg) if msg.contains("empty predicate")),
        "unexpected error: {err}"
    );
}

/// OR-ed constraints reach every element as one disjunction, and the filters
/// inside a constraint stay conjunctive. A caller authorized through two
/// grants must see the union of both, and neither alternative may widen the
/// other: the second constraint here narrows its tenant by resource id, so a
/// disjunction that lost the `AND` would admit that whole tenant.
///
/// Asserted per element rather than per statement, because the outer `WHERE`
/// carries the same scope — counting across the whole SQL would pass even with
/// an element body left unscoped.
#[test]
fn or_ed_constraints_reach_every_element_as_a_disjunction() {
    use toolkit_security::access_scope::{ScopeConstraint, ScopeFilter};

    let scope = AccessScope::from_constraints(vec![
        ScopeConstraint::new(vec![ScopeFilter::in_uuids(
            toolkit_security::access_scope::pep_properties::OWNER_TENANT_ID,
            vec![uuid::Uuid::from_u128(0x5150)],
        )]),
        ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(
                toolkit_security::access_scope::pep_properties::OWNER_TENANT_ID,
                vec![uuid::Uuid::from_u128(0x6161)],
            ),
            ScopeFilter::eq(
                toolkit_security::access_scope::pep_properties::RESOURCE_ID,
                toolkit_security::ScopeValue::Int(7),
            ),
        ]),
    ]);

    let sql = two_hop(&scope).expect("two resolvable constraints must compile");
    let bodies = element_bodies(&sql);
    assert_eq!(bodies.len(), 3, "expected three elements, got: {sql}");

    for (variable, body) in &bodies {
        assert!(
            body.contains(" OR "),
            "element {variable} lost the disjunction: {body}"
        );
        assert_eq!(
            body.matches("\"tenant_id\"").count(),
            2,
            "element {variable} must carry both alternatives\' tenant filter: {body}"
        );
        assert!(
            body.contains(" AND "),
            "element {variable} lost the conjunction inside the second constraint: {body}"
        );
    }
}

/// Policy 2 against the *live* scope: an entity may declare scope columns and
/// still resolve none of the properties this scope addresses. Compiled, that
/// would be a silent deny-all traversal; refused instead, naming the element
/// and the property.
#[test]
fn a_scope_no_constraint_of_which_resolves_is_refused() {
    let scope =
        AccessScope::from_constraints(vec![toolkit_security::access_scope::ScopeConstraint::new(
            vec![toolkit_security::access_scope::ScopeFilter::in_uuids(
                "department_id",
                vec![uuid::Uuid::from_u128(1)],
            )],
        )]);

    let err = two_hop(&scope).expect_err("an unresolvable scope must be refused");
    assert!(
        matches!(
            &err,
            ScopeError::UnresolvedScopeProperty { element, property }
                if *element == "a" && property == "department_id"
        ),
        "unexpected error: {err}"
    );
}

/// Policy 3 on the query path: `VertexOf` ties an entity to the graph *type*,
/// but only the declaration says what the graph object really contains. A
/// label the declaration does not register fails at build time, with a
/// message, rather than at the server.
#[test]
fn a_label_outside_the_declaration_is_refused() {
    struct Sparse;
    impl PropertyGraph for Sparse {
        const GRAPH_NAME: &'static str = "sparse";
        fn declaration() -> Result<GraphDeclaration, ScopeError> {
            GraphDeclaration::new::<Self>().vertex::<Self, node::Entity>(&["tenant_id", "id"])
        }
    }
    impl VertexOf<Sparse> for node::Entity {
        const LABEL: &'static str = "node";
    }
    // Registered nowhere in the declaration, yet the marker impl compiles —
    // exactly the drift the build-time check exists to catch.
    impl VertexOf<Sparse> for edge::Entity {
        const LABEL: &'static str = "ghost";
    }

    let err = node::Entity::find()
        .secure()
        .scope_with(&tenant_scope())
        .with_graph::<Sparse>()
        .match_path(|p| p.vertex::<edge::Entity>("g"))
        .column("g", "id", "x")
        .build_statement(sea_orm::DbBackend::Postgres)
        .expect_err("an undeclared label must be refused");
    assert!(
        matches!(err, ScopeError::Invalid(msg) if msg.contains("does not declare")),
        "unexpected error: {err}"
    );
}

/// A projected property missing from the element's `PROPERTIES` list is
/// *silently* unfilterable at the server, so the build refuses it by name.
#[test]
fn a_property_outside_the_declaration_is_refused() {
    let err = node::Entity::find()
        .secure()
        .scope_with(&tenant_scope())
        .with_graph::<Kb>()
        .match_path(|p| {
            p.vertex::<node::Entity>("a")
                .edge_to::<edge::Entity>("e")
                .to::<node::Entity>("b")
        })
        // `name` is a real column of the node table, deliberately not exposed.
        .column("b", "name", "n")
        .build_statement(sea_orm::DbBackend::Postgres)
        .expect_err("an unexposed property must be refused");
    assert!(
        matches!(err, ScopeError::Invalid(msg) if msg.contains("PROPERTIES")),
        "unexpected error: {err}"
    );

    let unbound = node::Entity::find()
        .secure()
        .scope_with(&tenant_scope())
        .with_graph::<Kb>()
        .match_path(|p| p.vertex::<node::Entity>("a"))
        .column("z", "id", "x")
        .build_statement(sea_orm::DbBackend::Postgres)
        .expect_err("a variable the pattern does not bind must be refused");
    assert!(
        matches!(unbound, ScopeError::Invalid(msg) if msg.contains("does not bind")),
        "unexpected error: {unbound}"
    );
}

/// A second `match_path` would silently replace the first pattern while keeping
/// the sibling relations it contributed — an uncorrelated `FROM` item nothing
/// references. Refused instead.
#[test]
fn a_second_match_path_is_refused() {
    let err = node::Entity::find()
        .secure()
        .scope_with(&tenant_scope())
        .with_graph::<Kb>()
        .match_path(|p| p.vertex::<node::Entity>("a"))
        .match_path(|p| p.vertex::<node::Entity>("b"))
        .column("a", "id", "x")
        .build_statement(sea_orm::DbBackend::Postgres)
        .expect_err("a second pattern must be refused");
    assert!(
        matches!(err, ScopeError::Invalid(msg) if msg.contains("twice")),
        "unexpected error: {err}"
    );
}

/// The middle case between "no predicate" and "correlated": a caller predicate
/// that never names the anchor. Keeping the anchor would reproduce the cross
/// join, so the anchor stays out unless the caller opts in explicitly.
#[test]
fn a_predicate_that_does_not_correlate_leaves_the_anchor_out() {
    use sea_orm::sea_query::{Alias, Expr};

    let sql = node::Entity::find()
        .secure()
        .scope_with(&tenant_scope())
        .with_graph::<Kb>()
        .match_path(|p| {
            p.vertex::<node::Entity>("a")
                .where_(
                    sea_orm::Condition::all()
                        .add(Expr::col((Alias::new("a"), Alias::new("id"))).eq(5_i64)),
                )
                .edge_to::<edge::Entity>("e")
                .to::<node::Entity>("b")
        })
        .column("b", "id", "neighbour")
        .build_statement(sea_orm::DbBackend::Postgres)
        .expect("builds")
        .sql;

    assert!(
        !sql.contains(r#""graph_node""#),
        "a predicate alone must not keep the anchor: {sql}"
    );
    assert!(
        element_bodies(&sql)["a"].contains(r#""a"."id" = "#),
        "the caller predicate itself is kept: {sql}"
    );
}

/// Policy 3 is about the *entity*, not the label: two entities may both
/// implement the marker trait under one label, and only the registered one is
/// what that label resolves to on the server. Patterning the other one would
/// compile scope from its columns while the traversal reads the registered
/// entity's table — refused, rather than quietly guarding the wrong table.
#[test]
fn a_label_bound_to_an_unregistered_entity_is_refused() {
    struct Shared;
    impl PropertyGraph for Shared {
        const GRAPH_NAME: &'static str = "shared_label";
        fn declaration() -> Result<GraphDeclaration, ScopeError> {
            GraphDeclaration::new::<Self>().vertex::<Self, node::Entity>(&["tenant_id", "id"])
        }
    }
    impl VertexOf<Shared> for node::Entity {
        const LABEL: &'static str = "node";
    }
    // Same label, different entity (and table), never registered.
    impl VertexOf<Shared> for edge::Entity {
        const LABEL: &'static str = "node";
    }

    let err = node::Entity::find()
        .secure()
        .scope_with(&tenant_scope())
        .with_graph::<Shared>()
        .match_path(|p| p.vertex::<edge::Entity>("x"))
        .column("x", "id", "n")
        .build_statement(sea_orm::DbBackend::Postgres)
        .expect_err("an unregistered entity must be refused even under a declared label");
    assert!(
        matches!(err, ScopeError::Invalid(msg) if msg.contains("other than the one")),
        "unexpected error: {err}"
    );
}
