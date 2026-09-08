use sea_orm::sea_query::{Alias, Query, SelectStatement, SimpleExpr};
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, sea_query::Expr};

use crate::secure::{AccessScope, ScopableEntity};
use toolkit_security::access_scope::{
    ScopeConstraint, ScopeFilter, ScopeValue, rg_tables, tenant_tables,
};
use toolkit_security::pep_properties;

/// Convert a [`ScopeValue`] to a `sea_query::SimpleExpr` for SQL binding.
fn scope_value_to_sea_expr(v: &ScopeValue) -> sea_orm::sea_query::SimpleExpr {
    match v {
        ScopeValue::Uuid(u) => Expr::value(*u),
        ScopeValue::String(s) => Expr::value(s.clone()),
        ScopeValue::Int(n) => Expr::value(*n),
        ScopeValue::Bool(b) => Expr::value(*b),
    }
}

/// Convert a slice of [`ScopeValue`] to `Vec<sea_orm::Value>` for IN clauses.
fn scope_values_to_sea_values(values: &[ScopeValue]) -> Vec<sea_orm::Value> {
    values
        .iter()
        .map(|v| match v {
            ScopeValue::Uuid(u) => sea_orm::Value::from(*u),
            ScopeValue::String(s) => sea_orm::Value::from(s.clone()),
            ScopeValue::Int(n) => sea_orm::Value::from(*n),
            ScopeValue::Bool(b) => sea_orm::Value::from(*b),
        })
        .collect()
}

/// Convert values for RG hierarchy columns, which are UUID-backed on
/// `PostgreSQL`. Valid UUID strings remain accepted for compatibility with
/// directly constructed filters; every other scalar shape fails closed before
/// SQL generation instead of producing a UUID/text operator error.
fn uuid_scope_values_to_sea_values(values: &[ScopeValue]) -> Option<Vec<sea_orm::Value>> {
    values
        .iter()
        .map(|value| match value {
            ScopeValue::Uuid(uuid) => Some(sea_orm::Value::from(*uuid)),
            ScopeValue::String(value) => {
                uuid::Uuid::parse_str(value).ok().map(sea_orm::Value::from)
            }
            ScopeValue::Int(_) | ScopeValue::Bool(_) => None,
        })
        .collect()
}

/// Build a deny-all condition (`WHERE false`).
fn deny_all() -> Condition {
    Condition::all().add(Expr::value(false))
}

/// Select the direct or closure-expanded group IDs used by a typed membership query.
enum GroupSelection {
    /// Match membership rows against explicit group UUIDs.
    Direct(Vec<sea_orm::Value>),
    /// Match membership rows against group IDs returned by a closure query.
    Subtree(Box<SelectStatement>),
}

/// Log a group filter that was compiled to a deny-all condition.
///
/// Fail-closed group filters are otherwise silent: the query simply returns
/// zero rows, which in production is indistinguishable from "the caller's
/// group grants match nothing". Only the predicate shape, property name, and
/// cause are logged — never group IDs or other filter values.
fn warn_group_filter_denied(predicate: &'static str, property: &str, cause: &'static str) {
    tracing::warn!(
        predicate,
        property,
        cause,
        "group scope filter compiled to deny-all"
    );
}

/// Resolve an external member-handle GTS schema ID to RG-local `gts_type.id` values.
fn membership_type_subquery(membership_resource_type: &str) -> SelectStatement {
    let member_type = Alias::new("member_type");
    Query::select()
        .column((member_type.clone(), Alias::new(rg_tables::GTS_TYPE_ID)))
        .from_as(Alias::new(rg_tables::GTS_TYPE_TABLE), member_type.clone())
        .and_where(
            Expr::col((member_type, Alias::new(rg_tables::GTS_TYPE_SCHEMA_ID)))
                .eq(membership_resource_type),
        )
        .to_owned()
}

/// Build the common typed RG membership condition for direct and subtree predicates.
///
/// RG stores external resource IDs as opaque text, so the entity property is
/// cast to text. Casting membership IDs to UUID would make valid non-UUID member
/// types raise a `PostgreSQL` conversion error. An absent member-handle type fails
/// closed because otherwise identical external IDs from different types could
/// be conflated.
fn typed_membership_condition(
    entity_property: SimpleExpr,
    membership_resource_type: &str,
    group_selection: GroupSelection,
) -> Option<SimpleExpr> {
    if membership_resource_type.is_empty() {
        return None;
    }

    let membership = Alias::new("membership");
    let group_id = Expr::col((
        membership.clone(),
        Alias::new(rg_tables::MEMBERSHIP_GROUP_ID),
    ));
    let group_predicate = match group_selection {
        GroupSelection::Direct(group_ids) => group_id.is_in(group_ids),
        GroupSelection::Subtree(closure_subquery) => group_id.in_subquery(*closure_subquery),
    };
    let mut membership_subquery = Query::select()
        .column((
            membership.clone(),
            Alias::new(rg_tables::MEMBERSHIP_RESOURCE_ID),
        ))
        .from_as(Alias::new(rg_tables::MEMBERSHIP_TABLE), membership.clone())
        .and_where(group_predicate)
        .to_owned();
    membership_subquery.and_where(
        Expr::col((membership, Alias::new(rg_tables::MEMBERSHIP_GTS_TYPE_ID)))
            .in_subquery(membership_type_subquery(membership_resource_type)),
    );

    Some(
        entity_property
            .cast_as(Alias::new("text"))
            .in_subquery(membership_subquery),
    )
}

/// Builds a `SeaORM` `Condition` from an `AccessScope` using property resolution.
///
/// # OR/AND Semantics
///
/// - Multiple constraints are OR-ed (alternative access paths)
/// - Filters within a constraint are AND-ed (all must match)
/// - Unknown `pep_properties` fail that constraint (fail-closed)
/// - If all constraints fail resolution, deny-all
///
/// # Policy Rules
///
/// | Scope | Behavior |
/// |-------|----------|
/// | deny-all (default) | `WHERE false` |
/// | unconstrained (allow-all) | No filtering (`WHERE true`) |
/// | single constraint | AND of resolved filters |
/// | multiple constraints | OR of ANDed filter groups |
pub fn build_scope_condition<E>(scope: &AccessScope) -> Condition
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
{
    if scope.is_unconstrained() {
        return Condition::all();
    }
    if scope.is_deny_all() {
        return deny_all();
    }

    let compiled: Vec<Condition> = scope
        .constraints()
        .iter()
        .filter_map(build_constraint_condition::<E>)
        .collect();

    match compiled.len() {
        0 => deny_all(),
        1 => compiled.into_iter().next().unwrap_or_else(deny_all),
        _ => {
            let mut or_cond = Condition::any();
            for c in compiled {
                or_cond = or_cond.add(c);
            }
            or_cond
        }
    }
}

/// Build SQL for a single constraint (AND of filters).
///
/// Returns `None` if any filter references an unknown property (fail-closed).
fn build_constraint_condition<E>(constraint: &ScopeConstraint) -> Option<Condition>
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
{
    if constraint.is_empty() {
        return Some(Condition::all());
    }

    // Resource groups are tenant-scoped. A group predicate without a tenant
    // filter in the same AND constraint would become an independent access
    // path when constraints are OR-ed, allowing group membership to escape the
    // mandatory tenant boundary. The single membership-type mapping also
    // describes only the entity resource ID, never another property. Enforce
    // both rules at the SQL compiler as well as the AuthZ response compiler so
    // direct `AccessScope` construction cannot bypass either invariant.
    let has_group_filter = constraint.filters().iter().any(|filter| {
        matches!(
            filter,
            ScopeFilter::InGroup(_) | ScopeFilter::InGroupSubtree(_)
        )
    });
    let has_tenant_filter = constraint.filters().iter().any(|filter| {
        filter.property() == pep_properties::OWNER_TENANT_ID
            && matches!(
                filter,
                ScopeFilter::Eq(_) | ScopeFilter::In(_) | ScopeFilter::InTenantSubtree(_)
            )
    });
    let has_group_filter_on_non_resource_property = constraint.filters().iter().any(|filter| {
        matches!(
            filter,
            ScopeFilter::InGroup(_) | ScopeFilter::InGroupSubtree(_)
        ) && filter.property() != pep_properties::RESOURCE_ID
    });
    if has_group_filter && (!has_tenant_filter || has_group_filter_on_non_resource_property) {
        let cause = if has_group_filter_on_non_resource_property {
            "group filter on a property other than the resource id"
        } else {
            "group filter without an owner_tenant_id filter in the same constraint"
        };
        let offending_property = constraint
            .filters()
            .iter()
            .find(|filter| {
                matches!(
                    filter,
                    ScopeFilter::InGroup(_) | ScopeFilter::InGroupSubtree(_)
                )
            })
            .map_or("", ScopeFilter::property);
        warn_group_filter_denied("InGroup/InGroupSubtree", offending_property, cause);
        return None;
    }

    let mut and_cond = Condition::all();
    for filter in constraint.filters() {
        let col = E::resolve_property(filter.property())?;
        match filter {
            ScopeFilter::Eq(eq) => {
                let expr = scope_value_to_sea_expr(eq.value());
                and_cond = and_cond.add(col.into_expr().eq(expr));
            }
            ScopeFilter::In(inf) => {
                let sea_values = scope_values_to_sea_values(inf.values());
                and_cond = and_cond.add(col.is_in(sea_values));
            }
            ScopeFilter::InGroup(gf) => {
                let Some(group_values) = uuid_scope_values_to_sea_values(gf.group_ids()) else {
                    warn_group_filter_denied("InGroup", gf.property(), "non-UUID group_ids");
                    and_cond = and_cond.add(Expr::value(false));
                    continue;
                };
                // An empty selector means "member of no group" — semantically
                // a deny; building the subquery would emit `IN ()`, a SQL
                // error on some engines.
                if group_values.is_empty() {
                    warn_group_filter_denied("InGroup", gf.property(), "empty group_ids");
                    and_cond = and_cond.add(Expr::value(false));
                    continue;
                }
                let Some(condition) = typed_membership_condition(
                    col.into_expr(),
                    gf.membership_resource_type(),
                    GroupSelection::Direct(group_values),
                ) else {
                    warn_group_filter_denied(
                        "InGroup",
                        gf.property(),
                        "empty membership resource type (untyped filter)",
                    );
                    and_cond = and_cond.add(Expr::value(false));
                    continue;
                };
                and_cond = and_cond.add(condition);
            }
            ScopeFilter::InGroupSubtree(sf) => {
                let Some(ancestor_values) = uuid_scope_values_to_sea_values(sf.ancestor_ids())
                else {
                    warn_group_filter_denied(
                        "InGroupSubtree",
                        sf.property(),
                        "non-UUID ancestor_ids",
                    );
                    and_cond = and_cond.add(Expr::value(false));
                    continue;
                };
                // See the InGroup arm: an empty selector is a deny, and the
                // closure subquery would otherwise emit `IN ()`.
                if ancestor_values.is_empty() {
                    warn_group_filter_denied("InGroupSubtree", sf.property(), "empty ancestor_ids");
                    and_cond = and_cond.add(Expr::value(false));
                    continue;
                }
                let closure = Alias::new("group_closure");
                let closure_subquery = Query::select()
                    .column((
                        closure.clone(),
                        Alias::new(rg_tables::CLOSURE_DESCENDANT_ID),
                    ))
                    .from_as(Alias::new(rg_tables::CLOSURE_TABLE), closure.clone())
                    .and_where(
                        Expr::col((closure, Alias::new(rg_tables::CLOSURE_ANCESTOR_ID)))
                            .is_in(ancestor_values),
                    )
                    .to_owned();
                let Some(condition) = typed_membership_condition(
                    col.into_expr(),
                    sf.membership_resource_type(),
                    GroupSelection::Subtree(Box::new(closure_subquery)),
                ) else {
                    warn_group_filter_denied(
                        "InGroupSubtree",
                        sf.property(),
                        "empty membership resource type (untyped filter)",
                    );
                    and_cond = and_cond.add(Expr::value(false));
                    continue;
                };
                and_cond = and_cond.add(condition);
            }
            ScopeFilter::InTenantSubtree(sf) => {
                // Respect-barriers (default), no descendant_status filter:
                //   col IN (SELECT descendant_id FROM tenant_closure
                //            WHERE ancestor_id = root_tenant_id AND barrier = 0)
                // Ignore-barriers:
                //   col IN (SELECT descendant_id FROM tenant_closure
                //            WHERE ancestor_id = root_tenant_id)
                // Non-empty descendant_status appends:
                //   AND descendant_status IN (...)
                //
                // The closure invariant guarantees `(ancestor=X, descendant=X)`
                // is always present (self-row, barrier=0), enforced by AM's
                // `ck_tenant_closure_self_row_barrier` check constraint, so the
                // root tenant is included regardless of the barrier clamp.
                //
                // The composite index
                // `idx_tenant_closure_ancestor_barrier_status (ancestor_id, barrier, descendant_status)`
                // covers all three clauses, so a status filter does not
                // change the access path.
                let root_expr = scope_value_to_sea_expr(sf.root_tenant_id());
                let mut subquery = Query::select()
                    .column(Alias::new(tenant_tables::CLOSURE_DESCENDANT_ID))
                    .from(Alias::new(tenant_tables::CLOSURE_TABLE))
                    .and_where(
                        Expr::col(Alias::new(tenant_tables::CLOSURE_ANCESTOR_ID)).eq(root_expr),
                    )
                    .to_owned();
                if sf.respect_barriers() {
                    subquery
                        .and_where(Expr::col(Alias::new(tenant_tables::CLOSURE_BARRIER)).eq(0_i16));
                }
                if !sf.descendant_status().is_empty() {
                    let status_values = scope_values_to_sea_values(sf.descendant_status());
                    subquery.and_where(
                        Expr::col(Alias::new(tenant_tables::CLOSURE_DESCENDANT_STATUS))
                            .is_in(status_values),
                    );
                }
                and_cond = and_cond.add(col.into_expr().in_subquery(subquery));
            }
        }
    }
    Some(and_cond)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use toolkit_security::access_scope::{ScopeConstraint, ScopeFilter, pep_properties};

    #[test]
    fn test_deny_all_scope() {
        let scope = AccessScope::default();
        assert!(scope.is_deny_all());
    }

    #[test]
    fn test_allow_all_scope() {
        let scope = AccessScope::allow_all();
        assert!(scope.is_unconstrained());
    }

    #[test]
    fn test_tenant_scope_not_empty() {
        let tid = uuid::Uuid::new_v4();
        let scope = AccessScope::for_tenant(tid);
        assert!(!scope.is_deny_all());
        assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, tid));
    }

    #[test]
    fn test_or_scope_has_multiple_constraints() {
        let t1 = uuid::Uuid::new_v4();
        let t2 = uuid::Uuid::new_v4();
        let r1 = uuid::Uuid::new_v4();

        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![
                ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![t1]),
                ScopeFilter::in_uuids(pep_properties::RESOURCE_ID, vec![r1]),
            ]),
            ScopeConstraint::new(vec![ScopeFilter::in_uuids(
                pep_properties::OWNER_TENANT_ID,
                vec![t2],
            )]),
        ]);
        assert_eq!(scope.constraints().len(), 2);
    }

    // --- Custom PEP property tests ---

    /// Test entity with a custom `department_id` property, mimicking what the
    /// derive macro generates for an entity with `pep_prop(department_id = "department_id")`.
    mod custom_prop_entity {
        use super::*;
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
        #[sea_orm(table_name = "custom_prop_test")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: Uuid,
            pub tenant_id: Uuid,
            pub department_id: Uuid,
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
                    "department_id" => Some(Column::DepartmentId),
                    _ => None,
                }
            }
        }
    }

    #[test]
    fn test_custom_property_resolves() {
        let dept = uuid::Uuid::new_v4();
        let scope =
            AccessScope::from_constraints(vec![ScopeConstraint::new(vec![ScopeFilter::in_uuids(
                "department_id",
                vec![dept],
            )])]);
        // Should produce a real condition (not deny-all) since the entity resolves "department_id".
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        // A deny-all condition contains `Expr::value(false)` — verify this is NOT that.
        let cond_str = format!("{cond:?}");
        assert!(
            !cond_str.contains("Value(Bool(Some(false)))"),
            "Expected a real condition, got deny-all: {cond_str}"
        );
    }

    #[test]
    fn test_unknown_property_deny_all() {
        let val = uuid::Uuid::new_v4();
        let scope =
            AccessScope::from_constraints(vec![ScopeConstraint::new(vec![ScopeFilter::in_uuids(
                "nonexistent",
                vec![val],
            )])]);
        // Unknown property should cause the constraint to fail → deny-all.
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            cond_str.contains("Value(Bool(Some(false)))"),
            "Expected deny-all, got: {cond_str}"
        );
    }

    #[test]
    fn test_eq_filter_produces_equality_condition() {
        let tid = uuid::Uuid::new_v4();
        let scope =
            AccessScope::from_constraints(vec![ScopeConstraint::new(vec![ScopeFilter::eq(
                pep_properties::OWNER_TENANT_ID,
                tid,
            )])]);
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        // Should produce an equality condition, not an IN condition
        assert!(
            !cond_str.contains("Value(Bool(Some(false)))"),
            "Expected a real condition, got deny-all: {cond_str}"
        );
    }

    const MEMBERSHIP_TYPE: &str = "gts.cf.core.rg.type.v1~example.core.rg.member.v1~";

    #[test]
    fn test_in_group_filter_produces_typed_text_subquery_condition() {
        let group_id = uuid::Uuid::new_v4();
        let tenant_id = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::in_group_typed(
                pep_properties::RESOURCE_ID,
                MEMBERSHIP_TYPE,
                vec![ScopeValue::Uuid(group_id)],
            ),
        ])]);
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        // Should NOT be deny-all
        assert!(
            !cond_str.contains("Value(Bool(Some(false)))"),
            "InGroup should produce a real condition, got: {cond_str}"
        );
        // Verify the condition references the membership table and columns
        assert!(
            cond_str.contains("resource_group_membership"),
            "InGroup condition must reference resource_group_membership table, got: {cond_str}"
        );
        assert!(
            cond_str.contains("group_id"),
            "InGroup condition must filter by group_id, got: {cond_str}"
        );
        assert!(
            cond_str.contains("resource_id"),
            "InGroup condition must join on resource_id, got: {cond_str}"
        );
        assert!(
            cond_str.contains("gts_type_id") && cond_str.contains("schema_id"),
            "InGroup condition must qualify the membership resource type, got: {cond_str}"
        );
        assert!(
            cond_str.contains(MEMBERSHIP_TYPE) && cond_str.contains("Cast"),
            "InGroup condition must cast the entity ID to text and bind the type, got: {cond_str}"
        );
    }

    #[test]
    fn test_typed_group_filter_without_tenant_constraint_denies_all() {
        let group_id = uuid::Uuid::new_v4();
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_group_typed(
            pep_properties::RESOURCE_ID,
            MEMBERSHIP_TYPE,
            vec![ScopeValue::Uuid(group_id)],
        )]));

        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            cond_str.contains("Value(Bool(Some(false)))"),
            "a group filter without tenant scope must fail closed, got: {cond_str}"
        );
    }

    #[test]
    fn test_direct_group_filter_on_non_resource_property_denies_all() {
        let tenant_id = uuid::Uuid::new_v4();
        let group_id = uuid::Uuid::new_v4();
        let scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::in_group_typed(
                "department_id",
                MEMBERSHIP_TYPE,
                vec![ScopeValue::Uuid(group_id)],
            ),
        ]));

        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            cond_str.contains("Value(Bool(Some(false)))"),
            "a group filter on a non-resource property must fail closed, got: {cond_str}"
        );
    }

    /// Unlike the legacy tests below (which deny earlier, at the
    /// constraint-shape guard, because they lack a tenant filter), this scope
    /// is well-formed apart from the missing membership type — so it reaches
    /// `typed_membership_condition`'s empty-type arm and pins that THAT arm
    /// denies, without ever emitting a membership subquery.
    #[test]
    #[allow(deprecated)]
    fn test_untyped_group_filter_with_tenant_denies_at_membership_arm() {
        let tenant_id = uuid::Uuid::new_v4();
        let group_id = uuid::Uuid::new_v4();
        let group_scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::in_group(
                pep_properties::RESOURCE_ID,
                vec![ScopeValue::Uuid(group_id)],
            ),
        ]));
        let subtree_scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::in_group_subtree(
                pep_properties::RESOURCE_ID,
                vec![ScopeValue::Uuid(group_id)],
            ),
        ]));

        for scope in [&group_scope, &subtree_scope] {
            let cond = build_scope_condition::<custom_prop_entity::Entity>(scope);
            let cond_str = format!("{cond:?}");
            assert!(
                cond_str.contains("Value(Bool(Some(false)))"),
                "an untyped group filter must fail closed at the membership arm, got: {cond_str}"
            );
            assert!(
                !cond_str.contains("resource_group_membership"),
                "no membership subquery may be emitted for an untyped filter, got: {cond_str}"
            );
        }
    }

    #[test]
    #[allow(deprecated)]
    fn test_legacy_untyped_in_group_filter_denies_all() {
        let group_id = uuid::Uuid::new_v4();
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_group(
            pep_properties::RESOURCE_ID,
            vec![ScopeValue::Uuid(group_id)],
        )]));

        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            cond_str.contains("Value(Bool(Some(false)))"),
            "an untyped group filter must fail closed, got: {cond_str}"
        );
    }

    /// An empty group selector means "member of no group" and must deny
    /// before SQL generation — building the membership subquery would emit
    /// `IN ()`, a SQL error on some engines. The PEP compiler rejects empty
    /// selectors upstream, so only directly constructed scopes reach this arm.
    #[test]
    fn test_empty_group_selectors_deny_before_sql_generation() {
        let tenant_id = uuid::Uuid::new_v4();
        let group_scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::in_group_typed(pep_properties::RESOURCE_ID, MEMBERSHIP_TYPE, vec![]),
        ]));
        let subtree_scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::in_group_subtree_typed(
                pep_properties::RESOURCE_ID,
                MEMBERSHIP_TYPE,
                vec![],
            ),
        ]));

        for scope in [&group_scope, &subtree_scope] {
            let cond = build_scope_condition::<custom_prop_entity::Entity>(scope);
            let cond_str = format!("{cond:?}");
            assert!(
                cond_str.contains("Value(Bool(Some(false)))"),
                "empty group selectors must fail closed, got: {cond_str}"
            );
            assert!(
                !cond_str.contains("resource_group_membership"),
                "no membership subquery may be emitted for an empty selector, got: {cond_str}"
            );
        }
    }

    #[test]
    fn test_malformed_direct_group_ids_deny_before_sql_generation() {
        let tenant_id = uuid::Uuid::new_v4();
        let group_scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::in_group_typed(
                pep_properties::RESOURCE_ID,
                MEMBERSHIP_TYPE,
                vec![ScopeValue::String("not-a-uuid".to_owned())],
            ),
        ]));
        let subtree_scope = AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::in_group_subtree_typed(
                pep_properties::RESOURCE_ID,
                MEMBERSHIP_TYPE,
                vec![ScopeValue::Int(7)],
            ),
        ]));

        for scope in [&group_scope, &subtree_scope] {
            let cond = build_scope_condition::<custom_prop_entity::Entity>(scope);
            let cond_str = format!("{cond:?}");
            assert!(
                cond_str.contains("Value(Bool(Some(false)))"),
                "malformed UUID hierarchy keys must fail closed, got: {cond_str}"
            );
        }
    }

    #[test]
    #[allow(deprecated)]
    fn test_legacy_untyped_in_group_subtree_filter_denies_all() {
        let ancestor_id = uuid::Uuid::new_v4();
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_group_subtree(
            pep_properties::RESOURCE_ID,
            vec![ScopeValue::Uuid(ancestor_id)],
        )]));

        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            cond_str.contains("Value(Bool(Some(false)))"),
            "an untyped group subtree filter must fail closed, got: {cond_str}"
        );
    }

    #[test]
    fn test_in_tenant_subtree_respects_barrier_by_default() {
        let root_id = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_tenant_subtree(
                pep_properties::OWNER_TENANT_ID,
                ScopeValue::Uuid(root_id),
                true,
                Vec::new(),
            ),
        ])]);
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            !cond_str.contains("Value(Bool(Some(false)))"),
            "InTenantSubtree should produce a real condition, got: {cond_str}"
        );
        assert!(
            cond_str.contains("tenant_closure"),
            "InTenantSubtree condition must reference tenant_closure table, got: {cond_str}"
        );
        assert!(
            cond_str.contains("ancestor_id"),
            "InTenantSubtree condition must filter by ancestor_id, got: {cond_str}"
        );
        assert!(
            cond_str.contains("descendant_id"),
            "InTenantSubtree condition must select descendant_id, got: {cond_str}"
        );
        assert!(
            cond_str.contains("barrier"),
            "Respect-barriers mode must clamp closure subquery with barrier=0, got: {cond_str}"
        );
    }

    #[test]
    fn test_in_tenant_subtree_ignore_barriers_omits_clamp() {
        let root_id = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_tenant_subtree(
                pep_properties::OWNER_TENANT_ID,
                ScopeValue::Uuid(root_id),
                false,
                Vec::new(),
            ),
        ])]);
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            cond_str.contains("tenant_closure"),
            "Ignore-barriers must still produce closure subquery, got: {cond_str}"
        );
        assert!(
            !cond_str.contains("barrier"),
            "Ignore-barriers must NOT clamp on barrier column, got: {cond_str}"
        );
    }

    #[test]
    fn test_in_tenant_subtree_binds_root_tenant_id() {
        // Single-root contract: the SQL must reference the root tenant UUID
        // exactly once via `ancestor_id = ?`, not an `IN` list. This guards
        // against accidental regressions to a multi-root encoding.
        let root_id = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_tenant_subtree(
                pep_properties::OWNER_TENANT_ID,
                ScopeValue::Uuid(root_id),
                true,
                Vec::new(),
            ),
        ])]);
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            cond_str.contains(&root_id.to_string()),
            "root tenant UUID must appear in the subquery, got: {cond_str}"
        );
    }

    #[test]
    fn test_in_tenant_subtree_with_descendant_status_emits_clause() {
        // Non-empty descendant_status must add `AND descendant_status IN (...)`
        // to the closure subquery, binding the SMALLINT values verbatim.
        let root_id = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_tenant_subtree(
                pep_properties::OWNER_TENANT_ID,
                ScopeValue::Uuid(root_id),
                true,
                vec![ScopeValue::Int(1), ScopeValue::Int(2)],
            ),
        ])]);
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            cond_str.contains("descendant_status"),
            "descendant_status clause must be present, got: {cond_str}"
        );
        // Status values are bound through sea-query as SMALLINT-compatible
        // integers — they appear as i64 placeholders in the debug print.
        assert!(
            cond_str.contains("BigInt(Some(1))") || cond_str.contains("Int(Some(1))"),
            "status value 1 must be bound, got: {cond_str}"
        );
        assert!(
            cond_str.contains("BigInt(Some(2))") || cond_str.contains("Int(Some(2))"),
            "status value 2 must be bound, got: {cond_str}"
        );
    }

    #[test]
    fn test_in_tenant_subtree_empty_descendant_status_omits_clause() {
        // Empty descendant_status must NOT add a status predicate — the
        // ignore-barriers variant exposes this most cleanly because then the
        // only mention of `barrier` or `descendant_status` would come from
        // the status clause itself.
        let root_id = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_tenant_subtree(
                pep_properties::OWNER_TENANT_ID,
                ScopeValue::Uuid(root_id),
                false,
                Vec::new(),
            ),
        ])]);
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            !cond_str.contains("descendant_status"),
            "empty descendant_status must NOT emit clause, got: {cond_str}"
        );
    }

    #[test]
    fn test_in_tenant_subtree_unknown_property_deny_all() {
        let root_id = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_tenant_subtree(
                "nonexistent",
                ScopeValue::Uuid(root_id),
                true,
                Vec::new(),
            ),
        ])]);
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            cond_str.contains("Value(Bool(Some(false)))"),
            "Unknown property must deny-all, got: {cond_str}"
        );
    }

    #[test]
    fn test_in_group_subtree_filter_produces_subquery_condition() {
        let ancestor_id = uuid::Uuid::new_v4();
        let tenant_id = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
            ScopeFilter::in_group_subtree_typed(
                pep_properties::RESOURCE_ID,
                MEMBERSHIP_TYPE,
                vec![ScopeValue::Uuid(ancestor_id)],
            ),
        ])]);
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            !cond_str.contains("Value(Bool(Some(false)))"),
            "InGroupSubtree should produce a real condition, got: {cond_str}"
        );
        // Verify subtree condition references hierarchy tables
        assert!(
            cond_str.contains("resource_group_membership"),
            "InGroupSubtree condition must reference resource_group_membership table, got: {cond_str}"
        );
        assert!(
            cond_str.contains("resource_id"),
            "InGroupSubtree condition must join on resource_id, got: {cond_str}"
        );
        assert!(
            cond_str.contains("gts_type_id")
                && cond_str.contains("schema_id")
                && cond_str.contains(MEMBERSHIP_TYPE)
                && cond_str.contains("Cast"),
            "InGroupSubtree must cast the entity ID and qualify membership type, got: {cond_str}"
        );
    }

    #[test]
    fn test_tenant_plus_in_group_produces_and_condition() {
        let tid = uuid::Uuid::new_v4();
        let gid = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tid]),
            ScopeFilter::in_group_typed(
                pep_properties::RESOURCE_ID,
                MEMBERSHIP_TYPE,
                vec![ScopeValue::Uuid(gid)],
            ),
        ])]);
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            !cond_str.contains("Value(Bool(Some(false)))"),
            "Combined tenant+group should produce a real condition, got: {cond_str}"
        );
    }

    #[test]
    fn test_in_tenant_subtree_and_eq_produces_and_condition() {
        let root_id = uuid::Uuid::new_v4();
        let resource_id = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_tenant_subtree(
                pep_properties::OWNER_TENANT_ID,
                ScopeValue::Uuid(root_id),
                true,
                Vec::new(),
            ),
            ScopeFilter::eq(pep_properties::RESOURCE_ID, resource_id),
        ])]);
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            cond_str.contains("tenant_closure"),
            "AND-composed condition must include closure subquery, got: {cond_str}"
        );
        assert!(
            cond_str.contains(&resource_id.to_string()),
            "AND-composed condition must include resource_id eq, got: {cond_str}"
        );
        assert!(
            !cond_str.contains("Value(Bool(Some(false)))"),
            "AND-composed condition must not deny-all, got: {cond_str}"
        );
    }

    #[test]
    fn test_in_tenant_subtree_or_with_in_produces_or_condition() {
        let root_id = uuid::Uuid::new_v4();
        let tenant_id = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![ScopeFilter::in_tenant_subtree(
                pep_properties::OWNER_TENANT_ID,
                ScopeValue::Uuid(root_id),
                true,
                Vec::new(),
            )]),
            ScopeConstraint::new(vec![ScopeFilter::in_uuids(
                pep_properties::OWNER_TENANT_ID,
                vec![tenant_id],
            )]),
        ]);
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            cond_str.contains("tenant_closure"),
            "OR condition must include closure subquery branch, got: {cond_str}"
        );
        assert!(
            cond_str.contains(&tenant_id.to_string()),
            "OR condition must include plain IN branch, got: {cond_str}"
        );
        assert!(
            !cond_str.contains("Value(Bool(Some(false)))"),
            "OR condition must not deny-all, got: {cond_str}"
        );
    }

    #[test]
    fn test_standard_plus_custom_scope() {
        let tid = uuid::Uuid::new_v4();
        let dept = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tid]),
            ScopeFilter::in_uuids("department_id", vec![dept]),
        ])]);
        // Both standard and custom pep_properties should resolve successfully.
        let cond = build_scope_condition::<custom_prop_entity::Entity>(&scope);
        let cond_str = format!("{cond:?}");
        assert!(
            !cond_str.contains("Value(Bool(Some(false)))"),
            "Expected a real condition, got deny-all: {cond_str}"
        );
    }
}
