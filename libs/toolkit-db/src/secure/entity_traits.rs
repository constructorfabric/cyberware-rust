use sea_orm::EntityTrait;

/// Defines the contract for entities that can be scoped by tenant, resource, owner, and type.
///
/// Each entity implementing this trait must explicitly declare all four scope dimensions:
/// - `tenant_col()`: Column for tenant-based isolation (multi-tenancy)
/// - `resource_col()`: Column for resource-level access (typically the primary key)
/// - `owner_col()`: Column for owner-based filtering
/// - `type_col()`: Column for type-based filtering
///
/// **Important**: No implicit defaults are allowed. Every scope dimension must be explicitly
/// specified as `Some(Column::...)` or `None` to enforce compile-time safety in secure systems.
///
/// # Example (Manual Implementation)
/// ```rust,ignore
/// impl ScopableEntity for user::Entity {
///     // The property-to-column mapping, written once. `resolve_property`
///     // and `scope_columns` read it (see `ScopeProperties`) and cannot be
///     // implemented per entity, so they cannot describe different sets.
///     const SCOPE_PROPERTIES: &'static [(&'static str, Self::Column)] = &[
///         ("owner_tenant_id", user::Column::TenantId),
///         ("id", user::Column::Id),
///     ];
///
///     fn tenant_col() -> Option<Self::Column> {
///         Some(user::Column::TenantId)
///     }
///     fn resource_col() -> Option<Self::Column> {
///         Some(user::Column::Id)
///     }
///     fn owner_col() -> Option<Self::Column> {
///         None
///     }
///     fn type_col() -> Option<Self::Column> {
///         None
///     }
/// }
/// ```
///
/// # Example (Using Derive Macro)
/// ```rust,ignore
/// use toolkit_db::secure::Scopable;
///
/// #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
/// #[sea_orm(table_name = "users")]
/// #[secure(
///     tenant_col = "tenant_id",
///     resource_col = "id",
///     no_owner,
///     no_type
/// )]
/// pub struct Model {
///     #[sea_orm(primary_key)]
///     pub id: Uuid,
///     pub tenant_id: Uuid,
///     pub email: String,
/// }
/// // Macro auto-generates SCOPE_PROPERTIES:
/// //   ("owner_tenant_id", Column::TenantId)   (from tenant_col)
/// //   ("id",              Column::Id)         (from resource_col)
/// ```
///
/// # Custom PEP Properties
/// ```rust,ignore
/// #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
/// #[sea_orm(table_name = "resources")]
/// #[secure(
///     tenant_col = "tenant_id",
///     resource_col = "id",
///     no_owner,
///     no_type,
///     pep_prop(department_id = "department_id"),
/// )]
/// pub struct Model {
///     #[sea_orm(primary_key)]
///     pub id: Uuid,
///     pub tenant_id: Uuid,
///     pub department_id: Uuid,
/// }
/// // Macro auto-generates SCOPE_PROPERTIES:
/// //   ("owner_tenant_id", Column::TenantId)       (from tenant_col)
/// //   ("id",              Column::Id)             (from resource_col)
/// //   ("department_id",   Column::DepartmentId)   (from pep_prop)
/// ```
///
/// # Unrestricted Entities
/// ```rust,ignore
/// #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
/// #[sea_orm(table_name = "system_config")]
/// #[secure(unrestricted)]
/// pub struct Model {
///     #[sea_orm(primary_key)]
///     pub id: Uuid,
///     pub config_key: String,
/// }
/// ```
pub trait ScopableEntity: EntityTrait {
    /// Indicates whether this entity is explicitly marked as unrestricted.
    ///
    /// This is a compile-time flag set via `#[secure(unrestricted)]` that documents
    /// the entity's global nature (e.g., system configuration, lookup tables).
    ///
    /// When `IS_UNRESTRICTED` is true, all column methods return `None`.
    ///
    /// Default: `false` (entity participates in scoping logic)
    const IS_UNRESTRICTED: bool = false;

    /// Returns the column that stores the tenant identifier.
    ///
    /// - Multi-tenant entities: `Some(Column::TenantId)`
    /// - Global/system entities: `None`
    ///
    /// Must be explicitly specified via `tenant_col = "..."` or `no_tenant`.
    fn tenant_col() -> Option<Self::Column>;

    /// Returns the column that stores the primary resource identifier.
    ///
    /// Typically the primary key column (e.g., `Column::Id`).
    ///
    /// Must be explicitly specified via `resource_col = "..."` or `no_resource`.
    fn resource_col() -> Option<Self::Column>;

    /// Returns the column that stores the resource owner identifier.
    ///
    /// Used for owner-based access control policies.
    ///
    /// Must be explicitly specified via `owner_col = "..."` or `no_owner`.
    fn owner_col() -> Option<Self::Column>;

    /// Returns the column that stores the resource type identifier.
    ///
    /// Used for type-based filtering in polymorphic scenarios.
    ///
    /// Must be explicitly specified via `type_col = "..."` or `no_type`.
    fn type_col() -> Option<Self::Column>;

    /// Every authorization property this entity understands, paired with the
    /// column that property means.
    ///
    /// The single place the mapping is written.
    /// [`ScopeProperties::resolve_property`] looks one property up in it and
    /// [`ScopeProperties::scope_columns`] lists its columns. Neither can be
    /// implemented per entity — see [`ScopeProperties`] — so the lookup and the
    /// list cannot describe different sets.
    ///
    /// Usually the tenant, resource and owner columns plus any `pep_prop(...)`
    /// columns, keyed by property name.
    /// [`type_col`](Self::type_col) belongs to no entry: no property name
    /// addresses it, so no scope can address it either.
    ///
    /// Two entries may name one column — a `pep_prop` pointing at the tenant
    /// column, say. The table stays as written and
    /// [`ScopeProperties::scope_columns`] then reports that column once per
    /// entry, because the list is a view of the table rather than a set. That
    /// is what the previous, separately-written list did too, and the only
    /// consumer that cares deduplicates: a property-graph declaration adds
    /// each column to an element's `PROPERTIES` once.
    ///
    /// `#[derive(Scopable)]` generates the table from the `#[secure(...)]`
    /// attributes. A manual implementation writes it out:
    ///
    /// ```rust,ignore
    /// const SCOPE_PROPERTIES: &'static [(&'static str, Self::Column)] = &[
    ///     (pep_properties::OWNER_TENANT_ID, Column::TenantId),
    ///     (pep_properties::RESOURCE_ID, Column::Id),
    ///     ("department_id", Column::DepartmentId),
    /// ];
    /// ```
    ///
    /// An unrestricted entity declares an empty table.
    const SCOPE_PROPERTIES: &'static [(&'static str, Self::Column)];
}

/// The two ways to read [`ScopableEntity::SCOPE_PROPERTIES`]: look one property
/// up, or list the columns.
///
/// Separate from [`ScopableEntity`] on purpose. The blanket implementation
/// below covers every scopable entity, so an entity **cannot** provide its own
/// version of either method: coherence rejects a second implementation
/// (`E0119`). That is what makes "the lookup and the list describe one set" an
/// invariant of the type system rather than a convention — the reason this
/// trait exists at all is that when both were overridable methods of
/// `ScopableEntity`, a hand-written entity could and did make them disagree
/// (issue #4726).
///
/// Callers get it for free with `E: ScopableEntity`; the trait only has to be
/// in scope:
///
/// ```rust,ignore
/// use toolkit_db::secure::{ScopableEntity, ScopeProperties};
///
/// fn column_for<E: ScopableEntity>(property: &str) -> Option<E::Column> {
///     E::resolve_property(property)
/// }
/// ```
pub trait ScopeProperties: ScopableEntity {
    /// Resolve an authorization property name to a database column.
    ///
    /// Maps PEP property names (e.g. `"owner_tenant_id"`) to `SeaORM` columns
    /// so the scope condition builder can translate `AccessScope` constraints
    /// into SQL `WHERE` clauses.
    ///
    /// A property the entity does not declare resolves to `None`. That is a
    /// runtime answer, not a compile-time guarantee: it is on the caller to
    /// treat it as deny. The compilers of this crate's own callers do so by
    /// dropping the constraint and falling to `WHERE false`.
    #[must_use]
    fn resolve_property(property: &str) -> Option<Self::Column> {
        Self::SCOPE_PROPERTIES
            .iter()
            .find(|(name, _)| *name == property)
            .map(|(_, column)| *column)
    }

    /// The columns a scope predicate can be compiled against: the columns of
    /// [`ScopableEntity::SCOPE_PROPERTIES`], in declaration order.
    ///
    /// One entry, one column, in order: a column two properties both name is
    /// reported twice. [`resolve_property`](Self::resolve_property) answers for
    /// one property and cannot be enumerated, and the SQL/PGQ graph declaration
    /// needs the whole set. A scope column left out of an element's `PROPERTIES` list
    /// cannot be filtered on inside `MATCH` (`docs/arch/secure-orm/ADR/0002`,
    /// Policy 3), and an element whose set is empty is refused up front rather
    /// than compiling to a deny-all traversal (Policy 2).
    #[must_use]
    fn scope_columns() -> Vec<Self::Column> {
        Self::SCOPE_PROPERTIES
            .iter()
            .map(|(_, column)| *column)
            .collect()
    }
}

/// Every scopable entity, and no room for a second implementation.
///
/// `tests/ui/fail/scope_properties_cannot_be_overridden.rs` pins that: an
/// entity trying to supply its own `resolve_property` fails to compile.
impl<E: ScopableEntity> ScopeProperties for E {}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::{ScopableEntity, ScopeProperties};
    use sea_orm::IdenStatic as _;
    use toolkit_security::access_scope::pep_properties;

    /// `DeriveEntityModel` does not derive `PartialEq` on `Column`, so columns
    /// are compared by the name they render as — which is also what the graph
    /// declaration writes into `PROPERTIES`.
    fn table_of<E: ScopableEntity>() -> Vec<(&'static str, &'static str)> {
        E::SCOPE_PROPERTIES
            .iter()
            .map(|(property, column)| (*property, column.as_str()))
            .collect()
    }

    fn listed_columns<E: ScopableEntity>() -> Vec<&'static str> {
        E::scope_columns()
            .iter()
            .map(sea_orm::IdenStatic::as_str)
            .collect()
    }

    /// Every dimension the derive maps, plus a custom property, so the table
    /// under test is not the trivial one.
    mod derived {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, toolkit_db_macros::Scopable)]
        #[sea_orm(table_name = "scope_properties_derived")]
        #[secure(
            tenant_col = "tenant_id",
            resource_col = "id",
            owner_col = "owner_id",
            type_col = "kind",
            pep_prop(department_id = "department_id")
        )]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: Uuid,
            pub tenant_id: Uuid,
            pub owner_id: Uuid,
            pub kind: String,
            pub department_id: Uuid,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    mod unrestricted {
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, toolkit_db_macros::Scopable)]
        #[sea_orm(table_name = "scope_properties_unrestricted")]
        #[secure(unrestricted)]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: Uuid,
            pub config_key: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}
    }

    /// A hand-written implementation, written the way the trait documents.
    /// Present so the property below is checked on the shape a gear writes,
    /// not only on the derive's output.
    mod manual {
        use sea_orm::entity::prelude::*;
        use toolkit_security::access_scope::pep_properties;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "scope_properties_manual")]
        pub struct Model {
            #[sea_orm(primary_key, auto_increment = false)]
            pub id: Uuid,
            pub tenant_id: Uuid,
            pub department_id: Uuid,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl crate::secure::ScopableEntity for Entity {
            const SCOPE_PROPERTIES: &'static [(&'static str, Self::Column)] = &[
                (pep_properties::OWNER_TENANT_ID, Column::TenantId),
                (pep_properties::RESOURCE_ID, Column::Id),
                ("department_id", Column::DepartmentId),
            ];

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
        }
    }

    /// What the derive builds out of `#[secure(...)]`: one entry per dimension
    /// that names a column, plus one per `pep_prop`, keyed by property name.
    #[test]
    fn the_derive_builds_the_table_from_the_attributes() {
        assert_eq!(
            table_of::<derived::Entity>(),
            vec![
                (pep_properties::OWNER_TENANT_ID, "tenant_id"),
                (pep_properties::RESOURCE_ID, "id"),
                (pep_properties::OWNER_ID, "owner_id"),
                ("department_id", "department_id"),
            ]
        );
    }

    /// The property this change exists for (#4726): the lookup and the list are
    /// two views of one table, so an entity cannot answer for a property whose
    /// column the list omits, nor list a column no property resolves to.
    ///
    /// Checked for the derive and for a hand-written implementation, because
    /// those were the two places that could drift when the two were written
    /// separately.
    #[test]
    fn the_lookup_and_the_list_are_two_views_of_one_table() {
        fn check<E: ScopableEntity>(what: &str) {
            let table = table_of::<E>();
            let listed = listed_columns::<E>();
            assert_eq!(
                listed.len(),
                table.len(),
                "{what}: the list must have one column per table entry"
            );
            for (index, (property, column)) in table.iter().enumerate() {
                assert_eq!(
                    E::resolve_property(property).map(|c| c.as_str()),
                    Some(*column),
                    "{what}: the lookup must answer every property the table declares"
                );
                assert_eq!(
                    listed[index], *column,
                    "{what}: the list must carry every column the table declares"
                );
            }
        }

        check::<derived::Entity>("derived");
        check::<manual::Entity>("manual");
        check::<unrestricted::Entity>("unrestricted");
    }

    /// A column two properties both name is reported once per entry, and the
    /// lookup answers for both names.
    ///
    /// The list is a view of the table, not a set — which is what the
    /// separately-written list did before this change too. The only consumer
    /// that cares deduplicates: a property-graph declaration adds each column
    /// to an element's `PROPERTIES` once, so a repeat costs nothing there.
    #[test]
    fn a_column_two_properties_name_is_listed_per_entry() {
        mod shared_column {
            use sea_orm::entity::prelude::*;

            // `nickname` is a second property for the tenant column.
            #[derive(
                Clone, Debug, PartialEq, Eq, DeriveEntityModel, toolkit_db_macros::Scopable,
            )]
            #[sea_orm(table_name = "scope_properties_shared_column")]
            #[secure(
                tenant_col = "tenant_id",
                no_resource,
                no_owner,
                no_type,
                pep_prop(nickname = "tenant_id")
            )]
            pub struct Model {
                #[sea_orm(primary_key, auto_increment = false)]
                pub id: Uuid,
                pub tenant_id: Uuid,
            }

            #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
            pub enum Relation {}

            impl ActiveModelBehavior for ActiveModel {}
        }

        assert_eq!(
            table_of::<shared_column::Entity>(),
            vec![
                (pep_properties::OWNER_TENANT_ID, "tenant_id"),
                ("nickname", "tenant_id"),
            ]
        );
        assert_eq!(
            listed_columns::<shared_column::Entity>(),
            vec!["tenant_id", "tenant_id"],
            "one column per entry, in order"
        );
        for property in [pep_properties::OWNER_TENANT_ID, "nickname"] {
            assert_eq!(
                shared_column::Entity::resolve_property(property).map(|c| c.as_str()),
                Some("tenant_id"),
                "both names must resolve to the shared column"
            );
        }
    }

    /// A property no entry names resolves to nothing, which every caller reads
    /// as fail-closed.
    #[test]
    fn an_undeclared_property_resolves_to_nothing() {
        assert!(derived::Entity::resolve_property("no_such_property").is_none());
        assert!(manual::Entity::resolve_property("no_such_property").is_none());
    }

    /// `type_col` is a dimension but not a scope property: no property name
    /// addresses it, so a scope cannot either. Were it in the table, an entity
    /// whose only dimension is `type_col` would pass the Policy 2 gates while
    /// resolving nothing (`docs/arch/secure-orm/ADR/0002`).
    #[test]
    fn the_type_column_is_not_a_scope_property() {
        assert_eq!(
            derived::Entity::type_col().map(|c| c.as_str()),
            Some("kind"),
            "the fixture must declare a type column for this to mean anything"
        );
        assert!(
            !table_of::<derived::Entity>()
                .iter()
                .any(|(_, column)| *column == "kind"),
            "the type column must not be addressable as a property"
        );
        assert!(!listed_columns::<derived::Entity>().contains(&"kind"));
    }

    /// An unrestricted entity has nothing to scope by, and says so with one
    /// empty table rather than with two separately-written empty answers.
    #[test]
    fn an_unrestricted_entity_declares_an_empty_table() {
        use unrestricted::Entity;

        const { assert!(unrestricted::Entity::IS_UNRESTRICTED) };
        assert!(Entity::SCOPE_PROPERTIES.is_empty());
        assert!(Entity::scope_columns().is_empty());
        assert!(Entity::resolve_property(pep_properties::OWNER_TENANT_ID).is_none());
    }
}
