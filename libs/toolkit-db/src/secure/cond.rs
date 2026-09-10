use sea_orm::sea_query::{Alias, Query, SelectStatement};
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, IdenStatic, sea_query::Expr};

use crate::secure::{AccessScope, ScopableEntity, ScopeError};
use toolkit_security::access_scope::{
    ScopeConstraint, ScopeFilter, ScopeValue, rg_tables, tenant_tables,
};

/// How a resolved column is written into SQL.
///
/// `ScopeFilter` arms operate on an `E::Column`, which renders qualified by the
/// entity's own table. A SQL/PGQ pattern element has no table reference — it has
/// a variable, and needs `dst.tenant_id`. Parameterising the addressing is what
/// lets one compiler serve ordinary selects, CTE bodies and graph elements
/// alike; a second, PGQ-specific compiler would double the number of places
/// tenant isolation could be wrong (`docs/arch/secure-orm/ADR/0002`).
// The graph-addressed half of this module is the API `docs/arch/secure-orm/ADR/0002`
// pins; `secure::pgq` is its consumer. That module is behind the `pgq` feature,
// so on a build without it the graph-only items really are unused — which is
// exactly what the `cfg_attr` allows below say, and nothing more. (These items
// are `pub` rather than `pub(crate)` only because `cond` is a private module,
// where the two are equivalent and `clippy::redundant_pub_crate` prefers the
// shorter one; nothing here is re-exported.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "pgq"), allow(dead_code))]
pub enum ColumnAddress {
    /// `"resources"."tenant_id"` — the entity's own table. Always renderable.
    Table,
    /// `dst.tenant_id` — a graph pattern variable.
    GraphElement {
        /// The pattern variable the column belongs to.
        var: &'static str,
        /// Whether the caller can place a correlated `FROM` item for the arms
        /// that need one.
        siblings: SiblingSupport,
    },
}

/// Whether a caller can host a correlated `FROM` item next to its query.
///
/// `PostgreSQL` 19 accepts no subquery inside a pattern predicate in any form,
/// so `InGroup`, `InGroupSubtree` and `InTenantSubtree` cannot be inlined into
/// a `MATCH`. They are still expressible, as a comma join with a correlated
/// reference — but only if the construct being built has room for that second
/// `FROM` item.
///
/// A caller that has no room must say so, and then those arms are an **error**.
/// Letting them drop would be fail-closed only in the letter: the constraint
/// vanishes, the remaining constraints collapse to `deny_all()`, and the query
/// returns nothing — a silent empty result that reads as missing data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiblingSupport {
    /// The caller can place correlated `FROM` items.
    Allowed,
    /// The caller cannot; subquery-producing arms fail loudly.
    ///
    /// This is also the documented v1 fallback if the correlated shape does not
    /// survive `PostgreSQL` 19 GA re-validation.
    Rejected,
}

/// A relation a compiled predicate correlates against.
///
/// Placed in the same `FROM` as the construct that references it — a comma join,
/// which `PostgreSQL` treats as an implicit lateral, so the correlation resolves
/// without `LATERAL` (which the parser refuses before `GRAPH_TABLE`).
#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "pgq"), allow(dead_code))]
pub struct SiblingSource {
    /// Alias the predicate refers to. Derived from the filter's position in the
    /// scope, so the same scope compiled for two pattern elements yields the
    /// same alias and the two references share one `FROM` item rather than
    /// duplicating it.
    pub alias: String,
    /// The relation itself.
    pub query: SelectStatement,
    /// Column of `alias` the predicate compares against. Read by the SQL
    /// assertions in this module's tests; the execution path bakes it into the
    /// predicate at compile time and never reads it back.
    #[cfg_attr(not(test), allow(dead_code))]
    pub column: &'static str,
}

/// A compiled scope predicate together with anything it references.
#[derive(Clone, Debug)]
pub struct ScopePredicate {
    condition: Condition,
    siblings: Vec<SiblingSource>,
}

impl ScopePredicate {
    /// The predicate. Read by this module's tests; the builders consume the
    /// predicate through [`Self::into_parts`] instead.
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn condition(&self) -> &Condition {
        &self.condition
    }

    /// Relations the predicate correlates against, if any. Test-only, as
    /// [`Self::condition`].
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn siblings(&self) -> &[SiblingSource] {
        &self.siblings
    }

    /// Split into the predicate and its correlated relations.
    #[cfg_attr(not(feature = "pgq"), allow(dead_code))]
    #[must_use]
    pub fn into_parts(self) -> (Condition, Vec<SiblingSource>) {
        (self.condition, self.siblings)
    }
}

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

/// Build a deny-all condition (`WHERE false`).
fn deny_all() -> Condition {
    Condition::all().add(Expr::value(false))
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
    // Table addressing renders every arm, so the `Result` cannot be `Err` here
    // and the sibling list is always empty. Kept as one code path so the select
    // path cannot drift from the graph path. Should a future arm break that
    // assumption, the fallback is still fail-closed — but it must not be
    // silent: a platform-wide `WHERE false` with nothing recording why is the
    // exact failure mode this module's documentation calls unacceptable.
    match build_scope_predicate::<E>(scope, ColumnAddress::Table) {
        Ok(predicate) => predicate.condition,
        Err(error) => {
            debug_assert!(
                false,
                "table addressing must render every ScopeFilter arm: {error}"
            );
            tracing::error!(
                %error,
                entity = %E::default().table_name(),
                "scope compilation failed under table addressing; \
                 falling back to a deny-all predicate"
            );
            deny_all()
        }
    }
}

/// Build a scope predicate with a chosen column addressing.
///
/// The generalisation of [`build_scope_condition`]. Under [`ColumnAddress::Table`]
/// this renders byte-identically to that function and never fails. Under
/// [`ColumnAddress::GraphElement`] the columns are qualified by a pattern
/// variable, and the arms that compile to a subquery are carried as a correlated
/// [`SiblingSource`] — or refused, if the caller declared it cannot host one.
///
/// # Errors
/// Returns [`ScopeError::Invalid`] when a filter cannot be expressed under
/// `address`. The failure is deliberately loud: dropping such a filter would
/// leave `deny_all()` and a silent empty result.
pub fn build_scope_predicate<E>(
    scope: &AccessScope,
    address: ColumnAddress,
) -> Result<ScopePredicate, ScopeError>
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
{
    if scope.is_unconstrained() {
        return Ok(ScopePredicate {
            condition: Condition::all(),
            siblings: Vec::new(),
        });
    }
    if scope.is_deny_all() {
        return Ok(ScopePredicate {
            condition: deny_all(),
            siblings: Vec::new(),
        });
    }

    let mut compiled: Vec<Condition> = Vec::new();
    let mut siblings: Vec<SiblingSource> = Vec::new();

    for (index, constraint) in scope.constraints().iter().enumerate() {
        // A constraint whose property does not resolve is dropped, as it always
        // has been — that is the fail-closed rule, and OR-ed constraints mean
        // one unresolvable alternative must not sink the others. What is *not*
        // dropped is a filter that resolves but cannot be rendered under this
        // addressing: that propagates.
        //
        // Siblings are collected per constraint and merged only if the whole
        // constraint survives. A constraint that pushed a sibling and then hit
        // an unresolvable property must leave no `FROM` item behind, or the
        // caller would place a relation nothing references.
        let mut constraint_siblings = Vec::new();
        if let Some(cond) =
            build_constraint_condition::<E>(constraint, address, index, &mut constraint_siblings)?
        {
            compiled.push(cond);
            siblings.append(&mut constraint_siblings);
        }
    }

    // A correlated sibling is placed as an unconditional comma join, so it
    // cannot serve a disjunction: with more than one surviving alternative, an
    // empty relation zeroes every branch — including the ones that never
    // referenced it — and a relation with k rows returns each other-branch
    // match k times. That holds whether the other alternatives need a sibling
    // of their own or not, so the condition is "a sibling exists and the OR has
    // more than one branch". Refused loudly rather than compiled wrong
    // (`docs/arch/secure-orm/ADR/0002`).
    if !siblings.is_empty() && compiled.len() > 1 {
        return Err(ScopeError::Invalid(
            "more than one OR-ed scope constraint survives and one of them needs a \
             correlated FROM item; a comma-joined sibling would zero or multiply \
             the other alternatives; narrow the scope or query per alternative",
        ));
    }

    let condition = match compiled.len() {
        0 => deny_all(),
        1 => compiled.into_iter().next().unwrap_or_else(deny_all),
        _ => {
            let mut or_cond = Condition::any();
            for c in compiled {
                or_cond = or_cond.add(c);
            }
            or_cond
        }
    };

    Ok(ScopePredicate {
        condition,
        siblings,
    })
}

/// Render a resolved column under `address`.
fn addressed<E>(col: E::Column, address: ColumnAddress) -> sea_orm::sea_query::Expr
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
{
    match address {
        // `into_expr` is what every existing arm used, so table addressing keeps
        // rendering exactly as before.
        ColumnAddress::Table => col.into_expr(),
        // A pattern variable is an identifier, so it goes through `Alias::new`,
        // which always escapes — never `format!`.
        ColumnAddress::GraphElement { var, .. } => {
            Expr::col((Alias::new(var), Alias::new(col.as_str())))
        }
    }
}

/// Whether this addressing can host a correlated `FROM` item.
const fn sibling_support(address: ColumnAddress) -> SiblingSupport {
    match address {
        // The ordinary select path inlines subqueries as it always has.
        ColumnAddress::Table => SiblingSupport::Allowed,
        ColumnAddress::GraphElement { siblings, .. } => siblings,
    }
}

/// One set-membership arm, described independently of how it will be rendered.
struct SetMembership {
    /// The relation the column must be a member of.
    query: SelectStatement,
    /// Column of that relation carrying the values.
    column: &'static str,
    /// Refusal to raise when the arm cannot be rendered where it was asked for.
    reject_msg: &'static str,
    /// Position of the filter in the scope, from which the correlated form
    /// derives its alias. Carried as indices rather than as the built string:
    /// this struct travels down the select path of every scoped query, where
    /// the alias is never read, and building it there would put an allocation
    /// on every filter of every query in the platform.
    constraint_index: usize,
    filter_index: usize,
    /// Whether the relation can contain duplicate keys.
    needs_distinct: bool,
}

/// Attach a set-membership arm, inline or as a correlated sibling.
///
/// Under table addressing the relation is inlined as `IN (subquery)`, which is
/// what the select path has always emitted. Under graph addressing it cannot be
/// — `PostgreSQL` rejects any subquery inside a pattern predicate — so the
/// relation moves to the same `FROM` and the predicate correlates against it.
fn attach_set_membership(
    and_cond: Condition,
    column: sea_orm::sea_query::Expr,
    address: ColumnAddress,
    arm: SetMembership,
    siblings: &mut Vec<SiblingSource>,
) -> Result<Condition, ScopeError> {
    match address {
        // Inlined exactly as the select path has always emitted it, without a
        // DISTINCT: `IN (subquery)` is a semi-join, so duplicates in the
        // subquery cannot multiply outer rows, and adding one here would move
        // SQL this generalisation is required not to move.
        ColumnAddress::Table => Ok(and_cond.add(column.in_subquery(arm.query))),
        ColumnAddress::GraphElement { .. } => {
            if sibling_support(address) == SiblingSupport::Rejected {
                return Err(ScopeError::Invalid(arm.reject_msg));
            }
            // Correlating turns the semi-join into a join, so a relation with
            // duplicate keys would return the same row once per duplicate.
            let mut query = arm.query;
            if arm.needs_distinct {
                query.distinct();
            }
            // Derived from the filter's position in the scope, so the same
            // scope compiled for two pattern elements produces the same alias —
            // the two references then share one `FROM` item instead of
            // duplicating the relation, which is what keeps a correlated join
            // from multiplying rows.
            let alias = format!("__cf_scope_{}_{}", arm.constraint_index, arm.filter_index);
            let correlated = Expr::col((Alias::new(alias.as_str()), Alias::new(arm.column)));
            let cond = and_cond.add(column.eq(correlated));
            siblings.push(SiblingSource {
                alias,
                query,
                column: arm.column,
            });
            Ok(cond)
        }
    }
}

/// Build SQL for a single constraint (AND of filters).
///
/// Returns `None` if any filter references an unknown property (fail-closed).
///
/// # Errors
/// Returns [`ScopeError::Invalid`] when a filter resolves but cannot be rendered
/// under `address` — which is different from, and must not be confused with, the
/// fail-closed drop above.
fn build_constraint_condition<E>(
    constraint: &ScopeConstraint,
    address: ColumnAddress,
    constraint_index: usize,
    siblings: &mut Vec<SiblingSource>,
) -> Result<Option<Condition>, ScopeError>
where
    E: ScopableEntity + EntityTrait,
    E::Column: ColumnTrait + Copy,
{
    if constraint.is_empty() {
        return Ok(Some(Condition::all()));
    }
    let mut and_cond = Condition::all();
    for (filter_index, filter) in constraint.filters().iter().enumerate() {
        let Some(col) = E::resolve_property(filter.property()) else {
            return Ok(None);
        };
        let column = addressed::<E>(col, address);

        match filter {
            ScopeFilter::Eq(eq) => {
                let expr = scope_value_to_sea_expr(eq.value());
                and_cond = and_cond.add(column.eq(expr));
            }
            ScopeFilter::In(inf) => {
                let sea_values = scope_values_to_sea_values(inf.values());
                and_cond = and_cond.add(column.is_in(sea_values));
            }
            ScopeFilter::InGroup(gf) => {
                // col IN (SELECT resource_id FROM resource_group_membership
                //          WHERE group_id IN (...))
                let group_values = scope_values_to_sea_values(gf.group_ids());
                let subquery = Query::select()
                    .column(Alias::new(rg_tables::MEMBERSHIP_RESOURCE_ID))
                    .from(Alias::new(rg_tables::MEMBERSHIP_TABLE))
                    .and_where(
                        Expr::col(Alias::new(rg_tables::MEMBERSHIP_GROUP_ID)).is_in(group_values),
                    )
                    .to_owned();
                and_cond = attach_set_membership(
                    and_cond,
                    column,
                    address,
                    SetMembership {
                        query: subquery,
                        column: rg_tables::MEMBERSHIP_RESOURCE_ID,
                        reject_msg: "scope filter InGroup needs a correlated FROM item, \
                                     which this query cannot host",
                        constraint_index,
                        filter_index,
                        // A resource authorized through two groups has two membership rows.
                        needs_distinct: true,
                    },
                    siblings,
                )?;
            }
            ScopeFilter::InGroupSubtree(sf) => {
                // col IN (SELECT resource_id FROM resource_group_membership
                //          WHERE group_id IN (
                //            SELECT descendant_id FROM resource_group_closure
                //            WHERE ancestor_id IN (...)
                //          ))
                let ancestor_values = scope_values_to_sea_values(sf.ancestor_ids());
                let closure_subquery = Query::select()
                    .column(Alias::new(rg_tables::CLOSURE_DESCENDANT_ID))
                    .from(Alias::new(rg_tables::CLOSURE_TABLE))
                    .and_where(
                        Expr::col(Alias::new(rg_tables::CLOSURE_ANCESTOR_ID))
                            .is_in(ancestor_values),
                    )
                    .to_owned();
                let membership_subquery = Query::select()
                    .column(Alias::new(rg_tables::MEMBERSHIP_RESOURCE_ID))
                    .from(Alias::new(rg_tables::MEMBERSHIP_TABLE))
                    .and_where(
                        Expr::col(Alias::new(rg_tables::MEMBERSHIP_GROUP_ID))
                            .in_subquery(closure_subquery),
                    )
                    .to_owned();
                and_cond = attach_set_membership(
                    and_cond,
                    column,
                    address,
                    SetMembership {
                        query: membership_subquery,
                        column: rg_tables::MEMBERSHIP_RESOURCE_ID,
                        reject_msg: "scope filter InGroupSubtree needs a correlated FROM item, \
                                     which this query cannot host",
                        constraint_index,
                        filter_index,
                        // Same reason: one resource can be a member of several groups in the subtree.
                        needs_distinct: true,
                    },
                    siblings,
                )?;
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
                //
                // No DISTINCT: the closure's primary key is
                // (ancestor_id, descendant_id), so descendants of one ancestor
                // are unique by construction and a correlated join against them
                // cannot multiply rows.
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
                and_cond = attach_set_membership(
                    and_cond,
                    column,
                    address,
                    SetMembership {
                        query: subquery,
                        column: tenant_tables::CLOSURE_DESCENDANT_ID,
                        reject_msg: "scope filter InTenantSubtree needs a correlated FROM item, \
                                     which this query cannot host",
                        constraint_index,
                        filter_index,
                        // The closure PK is (ancestor_id, descendant_id), so descendants of one ancestor are unique already.
                        needs_distinct: false,
                    },
                    siblings,
                )?;
            }
        }
    }
    Ok(Some(and_cond))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use toolkit_security::access_scope::{ScopeConstraint, ScopeFilter, pep_properties};

    // ─────────── addressing: the ADR-0002 test contract for `cond` ───────────
    //
    // Every assertion below is on **rendered SQL**, not on a `Condition`. A
    // predicate that never reaches the database would satisfy a `Debug`-form
    // assertion and still leak, which is why the CTE tests assert on built
    // statements and these do too.

    /// Render a bare condition into a statement, so assertions see what the
    /// server would.
    fn render(cond: &Condition) -> String {
        use sea_orm::sea_query::PostgresQueryBuilder;
        Query::select()
            .expr(Expr::value(1))
            .from(Alias::new("t"))
            .cond_where(cond.clone())
            .to_string(PostgresQueryBuilder)
    }

    fn render_sibling(sib: &SiblingSource) -> String {
        use sea_orm::sea_query::PostgresQueryBuilder;
        sib.query.to_string(PostgresQueryBuilder)
    }

    fn graph(siblings: SiblingSupport) -> ColumnAddress {
        ColumnAddress::GraphElement {
            var: "dst",
            siblings,
        }
    }

    const NIL: uuid::Uuid = uuid::Uuid::nil();

    fn one(filter: ScopeFilter) -> AccessScope {
        AccessScope::from_constraints(vec![ScopeConstraint::new(vec![filter])])
    }

    fn group_scope() -> AccessScope {
        one(ScopeFilter::in_group(
            pep_properties::RESOURCE_ID,
            vec![ScopeValue::Uuid(uuid::Uuid::from_u128(7))],
        ))
    }

    fn group_subtree_scope() -> AccessScope {
        one(ScopeFilter::in_group_subtree(
            pep_properties::RESOURCE_ID,
            vec![ScopeValue::Uuid(uuid::Uuid::from_u128(7))],
        ))
    }

    fn tenant_subtree_scope() -> AccessScope {
        one(ScopeFilter::in_tenant_subtree(
            pep_properties::OWNER_TENANT_ID,
            ScopeValue::Uuid(NIL),
            true,
            vec![],
        ))
    }

    /// Generalising the addressing must not move the select path. These are the
    /// exact strings the compiler emitted before the parameter existed, one per
    /// `ScopeFilter` arm.
    #[test]
    fn table_addressing_is_unchanged_for_every_arm() {
        use custom_prop_entity::Entity as E;

        let cases: Vec<(AccessScope, &str)> = vec![
            (
                AccessScope::for_tenant(NIL),
                r#"SELECT 1 FROM "t" WHERE "custom_prop_test"."tenant_id" IN ('00000000-0000-0000-0000-000000000000')"#,
            ),
            (
                one(ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, NIL)),
                r#"SELECT 1 FROM "t" WHERE "custom_prop_test"."tenant_id" = '00000000-0000-0000-0000-000000000000'"#,
            ),
            (
                group_scope(),
                r#"SELECT 1 FROM "t" WHERE "custom_prop_test"."id" IN (SELECT "resource_id" FROM "resource_group_membership" WHERE "group_id" IN ('00000000-0000-0000-0000-000000000007'))"#,
            ),
            (
                group_subtree_scope(),
                r#"SELECT 1 FROM "t" WHERE "custom_prop_test"."id" IN (SELECT "resource_id" FROM "resource_group_membership" WHERE "group_id" IN (SELECT "descendant_id" FROM "resource_group_closure" WHERE "ancestor_id" IN ('00000000-0000-0000-0000-000000000007')))"#,
            ),
            (
                tenant_subtree_scope(),
                r#"SELECT 1 FROM "t" WHERE "custom_prop_test"."tenant_id" IN (SELECT "descendant_id" FROM "tenant_closure" WHERE "ancestor_id" = '00000000-0000-0000-0000-000000000000' AND "barrier" = 0)"#,
            ),
        ];

        for (scope, expected) in cases {
            let predicate = build_scope_predicate::<E>(&scope, ColumnAddress::Table)
                .expect("table addressing renders every arm");
            assert_eq!(render(predicate.condition()), expected);
            assert!(
                predicate.siblings().is_empty(),
                "the select path must never be handed a FROM item to place"
            );
        }
    }

    /// The old entry point keeps its signature and its output.
    #[test]
    fn the_original_entry_point_still_agrees() {
        use custom_prop_entity::Entity as E;
        for scope in [
            AccessScope::for_tenant(NIL),
            group_scope(),
            tenant_subtree_scope(),
            AccessScope::deny_all(),
            AccessScope::allow_all(),
        ] {
            let direct = build_scope_condition::<E>(&scope);
            let viaparam = build_scope_predicate::<E>(&scope, ColumnAddress::Table)
                .expect("table addressing never fails");
            assert_eq!(render(&direct), render(viaparam.condition()));
        }
    }

    /// Graph addressing qualifies by the pattern variable, not by the table.
    #[test]
    fn graph_addressing_qualifies_by_the_variable() {
        use custom_prop_entity::Entity as E;
        let predicate = build_scope_predicate::<E>(
            &AccessScope::for_tenant(NIL),
            graph(SiblingSupport::Allowed),
        )
        .expect("an Eq arm inlines");
        let sql = render(predicate.condition());
        assert!(
            sql.contains(r#""dst"."tenant_id""#),
            "expected the variable-qualified column, got: {sql}"
        );
        assert!(
            !sql.contains("custom_prop_test"),
            "the table name must not appear in a pattern predicate: {sql}"
        );
    }

    /// A caller with nowhere to put a `FROM` item must be refused, not quietly
    /// handed a predicate with the filter dropped. This is the load-bearing one:
    /// a dropped filter renders as a perfectly valid query that returns nothing.
    #[test]
    fn subquery_arms_are_refused_when_no_sibling_can_be_placed() {
        use custom_prop_entity::Entity as E;
        for scope in [group_scope(), group_subtree_scope(), tenant_subtree_scope()] {
            let err = build_scope_predicate::<E>(&scope, graph(SiblingSupport::Rejected))
                .expect_err("a subquery arm cannot be inlined into a pattern");
            assert!(
                matches!(err, ScopeError::Invalid(msg) if msg.contains("correlated FROM item")),
                "unexpected error: {err}"
            );
        }
    }

    /// And the refusal is not the same thing as a deny-all. Asserting on the
    /// error alone would pass even if the compiler *also* emitted a deny-all
    /// traversal, which is the shape the dropped-filter bug produces.
    #[test]
    fn a_refusal_is_not_a_deny_all_query() {
        use custom_prop_entity::Entity as E;
        assert!(
            build_scope_predicate::<E>(&tenant_subtree_scope(), graph(SiblingSupport::Rejected))
                .is_err()
        );
        let deny = build_scope_predicate::<E>(&AccessScope::deny_all(), ColumnAddress::Table)
            .expect("deny-all compiles");
        assert!(
            render(deny.condition()).contains("FALSE"),
            "deny-all should render as a false predicate, got: {}",
            render(deny.condition())
        );
    }

    /// With room for a `FROM` item the subtree arms are servable: the predicate
    /// correlates against a sibling instead of inlining a subquery.
    #[test]
    fn subtree_arms_compile_to_a_correlated_sibling() {
        use custom_prop_entity::Entity as E;
        let predicate =
            build_scope_predicate::<E>(&tenant_subtree_scope(), graph(SiblingSupport::Allowed))
                .expect("a correlated sibling is expressible");

        let sql = render(predicate.condition());
        assert!(
            !sql.contains("SELECT \"descendant_id\""),
            "no subquery may appear inside a pattern predicate: {sql}"
        );
        assert_eq!(predicate.siblings().len(), 1);
        let sibling = &predicate.siblings()[0];
        assert!(
            sql.contains(&format!(r#""{}"."{}""#, sibling.alias, sibling.column)),
            "the predicate must reference the sibling it carries: {sql}"
        );
        assert!(render_sibling(sibling).contains("tenant_closure"));
    }

    /// Correlating turns a semi-join into a join, so a membership relation must
    /// be distinct on the correlated column or a resource authorized through two
    /// groups comes back twice.
    #[test]
    fn membership_siblings_are_distinct_and_the_closure_need_not_be() {
        use custom_prop_entity::Entity as E;

        for scope in [group_scope(), group_subtree_scope()] {
            let predicate = build_scope_predicate::<E>(&scope, graph(SiblingSupport::Allowed))
                .expect("expressible");
            let sql = render_sibling(&predicate.siblings()[0]);
            assert!(
                sql.starts_with("SELECT DISTINCT"),
                "a membership sibling must be distinct: {sql}"
            );
        }

        let closure =
            build_scope_predicate::<E>(&tenant_subtree_scope(), graph(SiblingSupport::Allowed))
                .expect("expressible");
        let sql = render_sibling(&closure.siblings()[0]);
        assert!(
            !sql.contains("DISTINCT"),
            "the closure is unique by primary key; a needless DISTINCT costs a sort: {sql}"
        );
    }

    /// Siblings from two *OR-ed* constraints cannot both be comma-joined: the
    /// relations are placed unconditionally, so an empty alternative zeroes the
    /// whole result and two non-empty ones multiply each match by the other's
    /// cardinality. Such a scope is refused for graph addressing — and still
    /// compiles for table addressing, where the subqueries inline.
    #[test]
    fn siblings_from_two_or_ed_constraints_are_refused() {
        use custom_prop_entity::Entity as E;
        let scope = AccessScope::from_constraints(vec![
            ScopeConstraint::new(vec![ScopeFilter::in_group(
                pep_properties::RESOURCE_ID,
                vec![ScopeValue::Uuid(uuid::Uuid::from_u128(7))],
            )]),
            ScopeConstraint::new(vec![ScopeFilter::in_tenant_subtree(
                pep_properties::OWNER_TENANT_ID,
                ScopeValue::Uuid(NIL),
                true,
                vec![],
            )]),
        ]);

        let err = build_scope_predicate::<E>(&scope, graph(SiblingSupport::Allowed))
            .expect_err("siblings from two alternatives must be refused");
        assert!(
            matches!(err, ScopeError::Invalid(msg) if msg.contains("more than one OR-ed")),
            "unexpected error: {err}"
        );
        assert!(
            build_scope_predicate::<E>(&scope, ColumnAddress::Table).is_ok(),
            "the select path inlines both subqueries and stays servable"
        );
    }

    /// The mixed case: only one alternative needs a sibling. The relation is
    /// still an unconditional comma join, so it zeroes or multiplies the
    /// alternative that never referenced it — refused all the same. A lone
    /// sibling-needing constraint stays servable, so the guard is about the
    /// disjunction, not about siblings as such.
    #[test]
    fn a_sibling_beside_a_plain_alternative_is_refused_too() {
        use custom_prop_entity::Entity as E;
        let needs_sibling = || {
            ScopeConstraint::new(vec![ScopeFilter::in_group(
                pep_properties::RESOURCE_ID,
                vec![ScopeValue::Uuid(uuid::Uuid::from_u128(7))],
            )])
        };
        let plain =
            ScopeConstraint::new(vec![ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, NIL)]);

        let mixed = AccessScope::from_constraints(vec![needs_sibling(), plain]);
        let err = build_scope_predicate::<E>(&mixed, graph(SiblingSupport::Allowed))
            .expect_err("a sibling beside a plain alternative must be refused");
        assert!(
            matches!(err, ScopeError::Invalid(msg) if msg.contains("more than one OR-ed")),
            "unexpected error: {err}"
        );
        assert!(
            build_scope_predicate::<E>(&mixed, ColumnAddress::Table).is_ok(),
            "the select path inlines the subquery and stays servable"
        );

        let alone = AccessScope::from_constraints(vec![needs_sibling()]);
        assert!(
            build_scope_predicate::<E>(&alone, graph(SiblingSupport::Allowed)).is_ok(),
            "one constraint with a sibling is the ordinary correlated case"
        );
    }

    /// The same scope compiled for two pattern elements must name the same
    /// sibling, so the two references share one `FROM` item. Two aliases would
    /// place the relation twice and multiply rows.
    #[test]
    fn the_same_scope_names_the_same_sibling_for_every_element() {
        use custom_prop_entity::Entity as E;
        let scope = tenant_subtree_scope();
        let a = build_scope_predicate::<E>(&scope, graph(SiblingSupport::Allowed)).expect("a");
        let b = build_scope_predicate::<E>(
            &scope,
            ColumnAddress::GraphElement {
                var: "src",
                siblings: SiblingSupport::Allowed,
            },
        )
        .expect("b");
        assert_eq!(a.siblings()[0].alias, b.siblings()[0].alias);
    }

    /// A constraint that produced a sibling and then hit an unresolvable
    /// property is dropped whole — and must leave no `FROM` item behind, or the
    /// caller would place a relation nothing references.
    #[test]
    fn a_dropped_constraint_leaves_no_sibling() {
        use custom_prop_entity::Entity as E;
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_tenant_subtree(
                pep_properties::OWNER_TENANT_ID,
                ScopeValue::Uuid(NIL),
                true,
                vec![],
            ),
            // This entity resolves no such property, so the whole constraint
            // drops (fail-closed) after the filter above already contributed.
            ScopeFilter::in_uuids("no_such_property", vec![NIL]),
        ])]);

        let predicate = build_scope_predicate::<E>(&scope, graph(SiblingSupport::Allowed))
            .expect("dropping is not an error");
        assert!(
            predicate.siblings().is_empty(),
            "a dropped constraint must not leave its sibling behind"
        );
        assert!(render(predicate.condition()).contains("FALSE"));
    }

    /// Allow-all and deny-all are addressing-independent: neither references a
    /// column, so neither can acquire a sibling or a variable qualifier.
    #[test]
    fn allow_all_and_deny_all_are_addressing_independent() {
        use custom_prop_entity::Entity as E;
        for scope in [AccessScope::allow_all(), AccessScope::deny_all()] {
            for address in [ColumnAddress::Table, graph(SiblingSupport::Rejected)] {
                let predicate =
                    build_scope_predicate::<E>(&scope, address).expect("no column to address");
                assert!(predicate.siblings().is_empty());
            }
        }
    }

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
            fn scope_columns() -> Vec<Column> {
                vec![Column::TenantId, Column::Id, Column::DepartmentId]
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

    #[test]
    fn test_in_group_filter_produces_subquery_condition() {
        let group_id = uuid::Uuid::new_v4();
        let scope =
            AccessScope::from_constraints(vec![ScopeConstraint::new(vec![ScopeFilter::in_group(
                pep_properties::RESOURCE_ID,
                vec![ScopeValue::Uuid(group_id)],
            )])]);
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
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_group_subtree(
                pep_properties::RESOURCE_ID,
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
    }

    #[test]
    fn test_tenant_plus_in_group_produces_and_condition() {
        let tid = uuid::Uuid::new_v4();
        let gid = uuid::Uuid::new_v4();
        let scope = AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
            ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tid]),
            ScopeFilter::in_group(pep_properties::RESOURCE_ID, vec![ScopeValue::Uuid(gid)]),
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
