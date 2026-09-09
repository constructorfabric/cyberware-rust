//! Compile-fail test: an entity cannot give `resolve_property` an answer that
//! its `SCOPE_PROPERTIES` table does not contain.
//!
//! `ScopeProperties` has a blanket implementation for every `ScopableEntity`,
//! so a second implementation for one entity is a coherence error. That is what
//! makes "the lookup and the column list describe one set" a property of the
//! type system rather than a convention someone has to remember (issue #4726).
//!
//! Security: the list is what a SQL/PGQ declaration turns into an element's
//! `PROPERTIES`. A lookup answering for a property whose column is not listed
//! there compiles a scope predicate the server cannot evaluate; the reverse
//! lets an entity pass the "resolves at least one scope column" gate while
//! resolving nothing. Both used to be reachable by hand.

use toolkit_db::secure::{ScopableEntity, ScopeProperties};

mod test_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sealed_scope_properties")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub department_id: Uuid,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    impl toolkit_db::secure::ScopableEntity for Entity {
        const SCOPE_PROPERTIES: &'static [(&'static str, Self::Column)] =
            &[("owner_tenant_id", Column::TenantId)];

        fn tenant_col() -> Option<Column> {
            Some(Column::TenantId)
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
    }
}

use test_entity::{Column, Entity};

// ERROR: `ScopeProperties` is already implemented for every `ScopableEntity`,
// so this entity cannot supply its own lookup — which is exactly the
// divergence the table was introduced to make impossible.
impl ScopeProperties for Entity {
    fn resolve_property(property: &str) -> Option<Column> {
        match property {
            // A column the table above does not list.
            "department_id" => Some(Column::DepartmentId),
            _ => None,
        }
    }
}

fn main() {
    let _ = <Entity as ScopableEntity>::SCOPE_PROPERTIES;
}
