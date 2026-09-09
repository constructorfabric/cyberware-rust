//! Compile-fail test: a graph query cannot be built from an unscoped query.
//!
//! ADR-0002's central claim is that tenant isolation for `GRAPH_TABLE` is a
//! *compile-time* guarantee, by the same construction as for CTEs: `with_graph`
//! is reachable only from `SecureSelect<E, Scoped>`, so a pattern element cannot
//! exist without a scope to embed into it. If `with_graph` were ever added to
//! the `Unscoped` impl block, every element would silently lose its predicate
//! and the traversal would read every tenant's rows — the outer `WHERE` would
//! trim the output while the walk had already crossed tenant boundaries.
//!
//! Security: this is the guard that makes "embed scope into every element
//! pattern" structural rather than a convention a reviewer has to enforce.

use sea_orm::entity::prelude::*;
use toolkit_db::secure::SecureEntityExt;
use toolkit_db::secure::pgq::{GraphDeclaration, PropertyGraph, VertexOf};
use toolkit_db::secure::ScopeError;

mod test_entity {
    use super::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "graph_unscoped_table")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i32,
        pub tenant_id: Uuid,
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
        fn resolve_property(_property: &str) -> Option<Column> {
            Some(Column::TenantId)
        }
        fn scope_columns() -> Vec<Column> {
            vec![Column::TenantId, Column::Id]
        }
    }
}

use test_entity::Entity;

struct TestGraph;

impl PropertyGraph for TestGraph {
    const GRAPH_NAME: &'static str = "test_graph";

    fn declaration() -> Result<GraphDeclaration, ScopeError> {
        GraphDeclaration::new::<Self>().vertex::<Self, Entity>(&["id"])
    }
}

impl VertexOf<TestGraph> for Entity {
    const LABEL: &'static str = "thing";
}

fn attempt_graph_without_scope() {
    // ERROR: `with_graph` is only available on the Scoped state.
    // `.scope_with(&scope)` must come first.
    let _graph_query = Entity::find().secure().with_graph::<TestGraph>();
}

fn main() {}
