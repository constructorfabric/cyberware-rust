---
status: accepted
date: 2026-08-18
decision-makers: Constructor Fabric steering committee
---

# ADR-0002: Safe Property-Graph Queries (SQL/PGQ) in the Secure ORM

**ID**: `cpt-cf-adr-secure-property-graph-policy`

## Context and Problem Statement

[ADR-0001](./0001-secure-cte-policy.md) settled the Secure ORM's load-bearing invariant —
**every access to a table passes through a scope condition** — and, for constructs that are
evaluated *independently* of the outer query, the rule that follows from it: **do not apply
scope around the construct, embed it inside**. That is why
[cte.rs](../../../../libs/toolkit-db/src/secure/cte.rs) scopes every CTE body rather than
the outer `WHERE`, and why the mechanism is structural — `SecureCteSelect` is reachable only
from `SecureSelect<E, Scoped>`, captures that query's `Arc<AccessScope>`, and re-scopes every
body from it
([cte.rs:229-249](../../../../libs/toolkit-db/src/secure/cte.rs#L229-L249)), so
a mixed-scope query is unrepresentable rather than rejected at runtime.

PostgreSQL 19 adds SQL/PGQ (ISO/IEC 9075-16): `CREATE PROPERTY GRAPH` declares a graph as
"a kind of read-only view over relational tables", and `GRAPH_TABLE (g MATCH … COLUMNS (…))`
queries it. The documentation is explicit that `GRAPH_TABLE` "acts like a table function in
that it produces a computed table as output", usable "anywhere you would use a subquery or
table", joinable, filterable and orderable from outside.

**This is the ADR-0001 hazard class exactly.** The outer query's scope predicate does not
reach inside `MATCH`:

```sql
-- UNSAFE: the traversal saw every tenant's rows; the outer WHERE only trims the output.
SELECT * FROM GRAPH_TABLE (
    mygraph MATCH (a IS resource)-[e IS depends_on]->(b IS resource)
            COLUMNS (a.id AS src, b.id AS dst)
) x
WHERE x.tenant_id = $1;
```

```sql
-- SAFE: every element pattern carries the scope predicate, so no element ever
-- held another tenant's rows.
SELECT * FROM GRAPH_TABLE (
    mygraph MATCH (a IS resource   WHERE a.tenant_id = $1)
                 -[e IS depends_on WHERE e.tenant_id = $1]->
                  (b IS resource   WHERE b.tenant_id = $1)
            COLUMNS (a.id AS src, b.id AS dst)
);
```

PostgreSQL permits `WHERE` inside an element pattern — `(a IS account WHERE a.type =
'savings')` — and that is the mechanism which makes the safe form expressible at all. It is
the direct analogue of embedding scope in a CTE body.

**Scope of this ADR.** As with ADR-0001, this policy governs **standard user gears** — the
handlers / services / repos that query their own domain tables. It does not govern
`toolkit-db` itself, nor migration infrastructure, where sea_query and hand-written SQL are
the implementation substrate.

**Why now, before there is a consumer.** Nothing in the repository uses SQL/PGQ today; a
repo-wide search for `GRAPH_TABLE` / `pgq` / property-graph vocabulary returns only
considered-and-rejected mentions of graph *databases*
([authorization/DESIGN.md:156](../../authorization/DESIGN.md#L156),
`gears/chat-engine/docs/ADR/0001-message-tree-structure.md:27`). But there is one tracked,
first-class requirement: IRM's `cpt-cf-infrastructure-resource-manager-fr-graph-query`
(`p2`, Planned —
[DESIGN.md:1345](../../../../gears/infrastructure-resource-manager/docs/DESIGN.md#L1345),
[PRD.md:843](../../../../gears/infrastructure-resource-manager/docs/PRD.md#L843)) over a
typed-edge table `resource_relationships`
([DESIGN.md:1107](../../../../gears/infrastructure-resource-manager/docs/DESIGN.md#L1107))
and a `resource_closure` read model, at a stated scale of 1M+ nodes and 5M+ edges. IRM is
still documentation only — zero `.rs` files — which is precisely the moment to fix the
policy, for the same reason ADR-0001 was written before ad-hoc CTEs accreted in gears.

**PostgreSQL 19 is pre-GA at the date of this ADR.** Beta 1 was released 2026-06-04, Beta 2
on 2026-07-16 with the feature set frozen, and GA is expected September/October 2026. This
ADR is therefore accepted as policy and as the target API, with the implementation gated
behind an opt-in feature flag (see "Backend gating") and required to be re-validated at GA.

## Decision Drivers

- **Preserve the invariant.** A graph query must be as impossible to construct unscoped as
  an `Unscoped` select is impossible to execute
  ([select.rs:151-188](../../../../libs/toolkit-db/src/secure/select.rs#L151-L188)).
- **No raw SQL in user-gear code** — the rule in
  [11_database_patterns.md:9](../../../toolkit_unified_system/11_database_patterns.md#L9)
  applies verbatim to `GRAPH_TABLE` and `CREATE PROPERTY GRAPH` strings.
- **One scope compiler, not two.** `build_scope_condition`
  ([cond.rs:54-189](../../../../libs/toolkit-db/src/secure/cond.rs#L54-L189)) is the single
  definition of "what scope means in SQL". A second, PGQ-specific compiler would be a second
  place for tenant isolation to be wrong.
- **Every pattern element must map to exactly one `ScopableEntity`.** Security is decided per
  entity by `resolve_property`
  ([entity_traits.rs:139-154](../../../../libs/toolkit-db/src/secure/entity_traits.rs#L139-L154));
  an element whose entity is ambiguous has an ambiguous security mapping.
- **A portability regression must be visible at compile time.** The Secure ORM emits the same
  SQL for Postgres, MySQL and SQLite today, and the CTE tests assert all three. `GRAPH_TABLE`
  exists on none but Postgres 19+.
- **Do not oversell the primitive.** PostgreSQL 19's initial implementation covers
  **fixed-depth patterns only**; variable-length paths are deferred to a later release. Any
  claim that PGQ supersedes ADR-0001's `recursive_cte` would be false.
- **Reviewability** — any escape from the safe path must be visible at the API surface.

## Considered Options

- **Option A** — a `SecureGraphSelect` reachable only from `SecureSelect<E, Scoped>`, whose
  pattern elements are **typed Rust entities**, with scope embedded into every element
  pattern.
- **Option B** — label-addressed pattern elements plus a runtime label → entity registry
  that supplies the scope condition.
- **Option C** — raw `GRAPH_TABLE` / `CREATE PROPERTY GRAPH` strings in gear code.
- **Option D** — decline SQL/PGQ; keep `recursive_cte` and closure tables.

## Decision Outcome

Chosen option: **Option A**, because it is the only option that keeps tenant isolation a
**compile-time** guarantee while adding no raw-SQL surface to gear code. The principle,
transposed from ADR-0001:

> **Do not apply scope outside `GRAPH_TABLE`. Embed scope into every element pattern
> participating in `MATCH` — vertices *and* edges alike.**

Then any element table a traversal touches is already filtered, and no element ever holds
another tenant's rows.

### Where the code lives

Two layers, with the security boundary unchanged:

```text
toolkit-sea-orm-pgq        knows HOW to express a WHERE inside MATCH
    (AST + renderer, security-agnostic: no AccessScope, no ScopableEntity,
     no DBRunner; models GRAPH_TABLE / MATCH / COLUMNS and the
     CREATE|DROP PROPERTY GRAPH DDL, renders to a sea_orm Statement)
        ^
        |
toolkit-db::secure::pgq    knows WHICH WHERE is mandatory
    (SecureGraphSelect, Arc<AccessScope>, typestate, scope injection into
     every element, execution through the sealed DBRunner)
        ^
        |
gear repositories
```

A separate crate is warranted because `GRAPH_TABLE` is a syntactic construct **sea_query does
not model at all** — verified against the pinned sea_query 1.0.2, where the only occurrences
of "graph" are in the `CYCLE`-clause documentation. The AST crate therefore fills a gap in
sea_query rather than duplicating it, and can be tested without any security context. It must
depend on `sea-orm`'s `sea_query` re-export rather than on `sea-query` directly: no crate in
this workspace depends on `sea-query` directly today, and a direct dependency could drift from
the version sea-orm resolves.

`toolkit-db` remains the security boundary. As with CTEs, a rendered `sea_orm::Statement` must
never become an execution API for gear code: execution goes through
`DBRunnerInternal::as_seaorm` and `FromQueryResult::find_by_statement`, exactly as
[cte.rs:604-660](../../../../libs/toolkit-db/src/secure/cte.rs#L604-L660) does, so no
`ConnectionTrait`, `DatabaseConnection` or `DatabaseTransaction` leaks
([runner.rs:20-71](../../../../libs/toolkit-db/src/secure/runner.rs#L20-L71)).

### API shape

Reachable only from a scoped select, mirroring `with_ctes()`. The concrete signatures are for
the implementation to pin; what this ADR fixes is that the entry point is on
`SecureSelect<E, Scoped>`, that every element is addressed by a `ScopableEntity`, and that
nothing in the surface accepts a bare label or a string of SQL.

```rust
impl<E: EntityTrait> SecureSelect<E, Scoped> {
    /// `G` is the property graph declaration (see "One declaration" below).
    pub fn with_graph<G: PropertyGraph>(self) -> SecureGraphSelect<E, G>;
}

impl<E: EntityTrait, G: PropertyGraph> SecureGraphSelect<E, G> {
    /// Each element is a typed entity, never a bare label. Scope is applied
    /// *after* the caller's predicate, so a predicate cannot filter it back off.
    pub fn match_path(self, f: impl FnOnce(PathBuilder<G>) -> PathBuilder<G>) -> Self;

    /// Project graph properties into the result via COLUMNS.
    pub fn columns(self, f: impl FnOnce(ColumnsBuilder<G>) -> ColumnsBuilder<G>) -> Self;

    /// Outer-query operations, on top of the scope the elements already carry.
    pub fn filter(self, filter: Condition) -> Self;
    pub fn limit(self, limit: u64) -> Self;
    pub fn distinct(self) -> Self;

    pub async fn all_as<T: FromQueryResult>(self, runner: &impl DBRunner)
        -> Result<Vec<T>, ScopeError>;
}
```

`PathBuilder` addresses elements as `.vertex::<resource::Entity>("src")`,
`.edge_to::<resource_link::Entity>("link")` / `.edge_from::<resource_link::Entity>("link")`,
`.to::<resource::Entity>("dst")`, each bounded `J: ScopableEntity`, and each contributing
`build_scope_condition::<J>` bound to *its own* variable. **Every edge carries an explicit
direction; there is no undirected element and no undirected shorthand** — it is orders of
magnitude slower for identical results (see "Feasibility constraints"), so an undirected hop
is written as two directed patterns. As with `join_cte`, the shape of the pattern is the
caller's correctness responsibility; it is not an isolation responsibility.

`SecureGraphSelect` must also be able to place a **second `FROM` item** next to the
`GRAPH_TABLE` clause, correlated from inside the pattern. This is not a gear-facing knob — the
scope compiler emits it — but it is a structural requirement on the builder, because it is the
only shape that carries a subtree scope into a pattern, and without room for it subtree scopes
are permanently unservable rather than merely unimplemented (see "Feasibility constraints").

### Policy 1 — no label-only matching

**A label is not a safe way to address a pattern element, and v1 does not expose one.** Two
documented properties of SQL/PGQ make it unsafe for this ORM:

- One label may span **several** element tables. From the PostgreSQL documentation, verbatim:
  *"Another use is to apply the same label to multiple element tables"* — the only requirement
  being that *"the properties match in number, name, and type"*. The worked example in the
  manual applies `LABEL person` to both `customers` and `employees`.
- Label disjunction is part of the pattern language: `(IS label1|label2)` matches either.

Security in this ORM is decided per entity — `E::resolve_property` maps a PEP property to a
column of one concrete table. A label-addressed element therefore potentially has *several
different* security mappings, and the builder would have to pick one. Every pattern element is
consequently addressed by a Rust entity type, and the emitted `IS` names a label the platform
itself declared 1:1 for that element table. Sharing a label across element tables is a
property-graph *DDL* choice, and the generated DDL (Policy 3) does not make it.

### Policy 2 — every element table must resolve a scope property

This is the constraint that the existing code makes non-negotiable, and it is easy to miss
because getting it wrong **fails closed and therefore looks like it works**.

`build_scope_condition` is fail-closed by construction: a filter whose property does not
resolve drops its whole constraint
([cond.rs:98](../../../../libs/toolkit-db/src/secure/cond.rs#L98)), and if every constraint
drops, the result is `deny_all()` — `WHERE false`
([cond.rs:66-83](../../../../libs/toolkit-db/src/secure/cond.rs#L66-L83)). It does **not**
consult `ScopableEntity::IS_UNRESTRICTED`; that flag short-circuits only the write path
([db_ops.rs:68](../../../../libs/toolkit-db/src/secure/db_ops.rs#L68)).

Now consider the platform's existing edge-shaped tables. Every one of them declares no
resolvable scope dimension:

| Table | Entity | `#[secure(...)]` |
|---|---|---|
| `tenant_closure` | [tenant_closure.rs:32](../../../../gears/system/account-management/account-management/src/infra/storage/entity/tenant_closure.rs#L32) | `no_tenant, no_resource, no_owner, no_type` |
| `resource_group_closure` | [resource_group_closure.rs:9](../../../../gears/system/resource-group/resource-group/src/infra/storage/entity/resource_group_closure.rs#L9) | `no_tenant, no_resource, no_owner, no_type` |
| `resource_group_membership` | [resource_group_membership.rs:10](../../../../gears/system/resource-group/resource-group/src/infra/storage/entity/resource_group_membership.rs#L10) | `no_tenant, no_resource, no_owner, no_type` |
| `gts_type_allowed_parent` | `.../entity/gts_type_allowed_parent.rs` | `no_tenant, no_resource, no_owner, no_type` |
| `gts_type_allowed_membership` | `.../entity/gts_type_allowed_membership.rs` | `no_tenant, no_resource, no_owner, no_type` |

This is deliberate and correct for those tables — a closure row references *two* tenants, so
there is no single row-owning tenant to filter on. But it means that used as a graph element
under any constrained scope, such a table compiles to `WHERE false`: the traversal is safe and
returns **zero rows, silently**. The tempting fix — relaxing the edge table to `unrestricted`,
or letting the graph builder skip scope for "join-only" elements — is the actual isolation
hole this ADR exists to prevent.

**Policy:** every element table in a secure property graph, vertex *and* edge, must resolve at
least one scope property under the query's scope. The builder **rejects** an element that
cannot, loudly, rather than emitting a deny-all traversal. Whether that rejection is a
compile-time bound or a `ScopeError` at build time is an implementation choice — the
requirement is that it is not silent.

**Eligibility must be checked before compiling, and inspecting the compiled output is not a
substitute.** Confirmed on the probe branch: an entity that resolves no property compiles to
exactly the same condition under `for_tenant(t)` as a legitimately denying scope does under
`deny_all()`. The failure happens inside `resolve_property`, not in a filter arm, so by the
time there is a `Condition` the two cases are indistinguishable — a post-hoc "is this
deny-all?" check would either miss the bug or reject correct deny-all queries. The check has to
run against the entity's scope-property set, before any condition is built.

Consequences worth stating:

- **Today's closure and membership tables are not eligible graph elements.** Traversing a
  tenant or resource-group hierarchy stays with `InTenantSubtree` / `InGroupSubtree`, which
  already compile to a flat closure subquery
  ([cond.rs:121-185](../../../../libs/toolkit-db/src/secure/cond.rs#L121-L185)), or with
  `recursive_cte`.
- **The driving use case satisfies this.** IRM states that `resource_relationships` and
  `resource_closure` *"carry `tenant_id` and are filtered directly"*
  ([DESIGN.md:993](../../../../gears/infrastructure-resource-manager/docs/DESIGN.md#L993)).
  A gear that wants a secure property graph must model its edge table the same way: a
  tenant-bearing edge row, not a bare pair of foreign keys.

### Policy 3 — one declaration for both DDL and query

A property graph is a schema object, created by a migration. Nothing in Rust otherwise
verifies that the label `resource` in a `MATCH` maps to the table behind
`resource::Entity` — and worse, a `MATCH` predicate references **properties**, not columns, so
a graph declared `PROPERTIES (name)` cannot be scope-filtered at all: the scope column is
simply not visible to the pattern language. The failure mode is quiet — a column left out of
the DDL's `PROPERTIES` list does not make the graph invalid, it just makes that column
unfilterable — so nothing except generation keeps the two sides in step.

Both problems disappear if a single Rust declaration — a `PropertyGraph` trait, ideally
derived — is the source of **both** the `CREATE PROPERTY GRAPH` DDL and the query AST:

- label ↔ entity becomes compiler-checked rather than a convention two files apart;
- the DDL generator can guarantee that every element table exposes its scope columns
  (`tenant_col`, `resource_col`, `owner_col`, `type_col`, `pep_prop(...)`) as properties under
  their column names, which is the precondition for Policy 2 being satisfiable;
- a gear cannot declare a label shared across element tables (Policy 1) because the generator
  does not emit one.

The migration executes that generated DDL. Raw SQL there is allowed and is already the
established idiom for statements sea-orm-migration cannot model —
`conn.execute_unprepared(...)` with per-backend branching appears in 31 migration files, e.g.
`gears/file-storage/file-storage/src/infra/storage/migrations/m20260707_000001_content_hash_modes.rs:91`.
Because `GRAPH_TABLE` is Postgres-only, such a migration must no-op on other backends rather
than fail.

### Addressing a scope condition by graph variable

This is the one substantive change required in the Secure ORM core. `build_scope_condition`
emits a condition bound to the entity's own table (`resources.tenant_id`), because
`ScopeFilter` arms operate on an `E::Column`
([cond.rs:88-189](../../../../libs/toolkit-db/src/secure/cond.rs#L88-L189)) and a CTE body can
simply reuse the ordinary path — `cte()` routes through `scope_with_arc` precisely so that
"apply scope to a select" has exactly one definition
([cte.rs:279-298](../../../../libs/toolkit-db/src/secure/cte.rs#L279-L298)).

A pattern element has no table reference; it has a variable, and needs `dst.tenant_id`. The
required change is therefore to parameterise *how a resolved column is addressed*, not to
duplicate the compiler:

```text
AccessScope
    |
    v
ScopableEntity::resolve_property()      <- unchanged: which column means what
    |
    v
column addressing                        <- the only new degree of freedom
    for_table()          -> "resources"."tenant_id"       (always Ok)
    for_graph_element(v) -> dst.tenant_id                 (fallible: see below)
    |
    v
scope predicate
```

Concretely: give `build_scope_condition` a column-addressing parameter (equivalently, a
`ScopeConditionBuilder::<E>` with `for_table()` and `for_graph_element("dst")`), and have both
the ordinary select path and the PGQ path go through it. Writing a second PGQ-specific scope
compiler is explicitly rejected: it would double the number of places where tenant isolation
could be wrong, and the two would drift as `ScopeFilter` gains variants.

**The addressing function is fallible, and that is part of the API this ADR pins.** Under graph
addressing the three subquery-producing arms cannot be rendered into a pattern predicate at all
(see "Feasibility constraints"), and they must fail *loudly*. Letting them drop is fail-closed
only in the letter: the constraint vanishes, the remaining constraints collapse to `deny_all()`
([cond.rs:66-83](../../../../libs/toolkit-db/src/secure/cond.rs#L66-L83)), and the traversal
returns nothing — the silent empty result Policy 2 exists to prevent, arrived at by a different
route. So the signature is `-> Result<ScopePredicate, ScopeError>`, where `ScopePredicate`
carries a `Condition` plus, for the correlated-sibling shape, the `FROM` item that condition
references. Table addressing returns `Ok` for every arm and must render byte-identically to
today's SQL, so generalising the addressing does not move the existing select path. This changes
the *shape* of the function rather than its internals, which is why it is fixed here instead of
left to the implementation.

### Backend gating

`GRAPH_TABLE` exists only on PostgreSQL 19+. The whole PGQ surface is therefore behind a
`pgq` Cargo feature that **implies** `pg`, so a gear cannot even name the types on a build
without a Postgres backend. This is deliberately a compile-time incompatibility rather than a
runtime error: the Secure ORM's CTE support is backend-neutral and asserted on all three
backends, whereas a graph query is not portable at all, and discovering that from a runtime
`ScopeError` in production would be strictly worse.

The crate already has the precedent for backend-gated public API — `#[cfg(feature = "pg")] pub
fn postgres_lazy` ([advisory_locks.rs:1507](../../../../libs/toolkit-db/src/advisory_locks.rs#L1507))
and `#[cfg(feature = "sqlite")] pub mod sqlite_pragma`
([options.rs:181](../../../../libs/toolkit-db/src/options.rs#L181)) — and the feature block to
extend ([Cargo.toml:19-56](../../../../libs/toolkit-db/Cargo.toml#L19-L56)). Note
`metadata.docs.rs.all-features = true`, so the gated code must compile in the docs build.

### Feasibility constraints (must be honored by the implementation)

ADR-0001 carried a section of this name because an implementer who assumed `Select<E>::with()`
existed would hit a type wall. The equivalents here are below; the measured ones were checked
against a live PostgreSQL 19 beta 2 server by @vasylcf (probe branch
`vasylcf/gears-rust:feature/pgq-adr0002-probe`, scripts and results under
`gears/graph-storage/dev/adr0002-probe/`) and must be re-confirmed at GA:

- **Settled: an element-pattern `WHERE` may not contain a subquery.** The platform's principal
  scope shapes do not compile to simple comparisons — `InGroup`, `InGroupSubtree` and
  `InTenantSubtree` all compile to `col IN (SELECT …)` over the membership/closure tables
  ([cond.rs:108-185](../../../../libs/toolkit-db/src/secure/cond.rs#L108-L185)). Both
  `col IN (SELECT …)` and `col = ANY(ARRAY(SELECT …))` are rejected outright:

  ```text
  ERROR:  subqueries within GRAPH_TABLE reference are not supported
  ```

  `CROSS JOIN LATERAL GRAPH_TABLE (…)` is refused too, with a bare syntax error — `LATERAL` is
  not accepted before the construct. So no subquery-producing scope shape can be injected into
  a pattern predicate in any form, and only `Eq`/`In` are directly inlinable.

- **But subtree scopes are still servable: a correlated sibling `FROM` item reaches the
  pattern.** A comma join with a correlated reference — an implicit lateral — is accepted:

  ```sql
  WITH closure(descendant_id) AS (
      SELECT descendant_id FROM tenant_closure WHERE ancestor_id = $1
  )
  SELECT g.n FROM closure c,
    GRAPH_TABLE (kb MATCH (a IS node)-[e IS edge]->(b IS node)
                    WHERE a.id = $2
                      AND a.tenant_id = c.descendant_id
                      AND b.tenant_id = c.descendant_id
                    COLUMNS (b.id AS n)) g;
  ```

  The closure stays in the same statement, so this costs neither an extra round trip nor an
  unbounded `IN` list — fallback 2 without either of its two costs. Measured at 0.337 ms on a
  199k-node fixture, planned as nested loops over index scans; with a three-tenant closure the
  per-tenant semantics are right and rows do not multiply, because the tenant predicate pins
  each match to exactly one closure row. Two obligations follow:

  - **The correlated sibling must be distinct on the correlated column.** Correlating turns a
    semi-join into a join, so a sibling carrying duplicate keys multiplies rows — a resource
    authorized through two groups would appear twice in a membership listing. `tenant_closure`
    descendants are already distinct per ancestor; `resource_group_membership` resource ids are
    not, so the `InGroup` / `InGroupSubtree` siblings need an explicit `DISTINCT`. Pin it with a
    test.
  - Scope stays per element: each element carries its own correlated predicate against the same
    sibling. The sibling is a source of values, never a substitute for scoping an element.

  Two kinds of sibling are in play, and only the second needs new API surface:

  - **The anchor entity `E` itself.** Because a pattern can correlate against any plain
    relation in the same `FROM`, a pattern may reference `E`'s own columns with no new
    construct at all — a traversal seeded from an already-scoped entity query. This turns
    Option A's forced anchor from a pure cost into the mechanism for the most common shape,
    "start from these resources, walk out one hop".
  - **An arbitrary relation.** A tenant-subtree scope needs exactly this and cannot get it any
    other way: the closure is neither `E` nor a graph element, and it cannot be promoted to the
    anchor either, because a scoped select over `tenant_closure` compiles to `WHERE false` by
    Policy 2. So the question "may `with_graph()` hold a sibling `FROM` item?" is precisely the
    question "are subtree scopes servable or permanently on fallback 1?" — which is why it is
    settled here rather than during implementation.

  If this shape does not survive GA re-validation, the fallbacks, in preference order:
  1. Reject the subquery-producing scope shapes with `ScopeError` in v1 and document
     `recursive_cte` / closure-based scoping as the route for subtree scopes. Fails closed and
     keeps one scope compiler.
  2. Pre-resolve the closure to a literal id list and inline it. Semantically equivalent and
     always expressible, but costs an extra round trip and produces an unbounded `IN` list —
     acceptable only with a cap and a documented failure mode when exceeded.

- **Never emit an undirected element.** The undirected shorthand `(a)-[e]-(b)` plans as a
  parallel sequential scan of the edge table. Measured on the probe fixture, for identical
  result sets:

  | pattern | rows | time |
  |---|---:|---:|
  | one undirected element | 10 | 735 ms |
  | two undirected elements | 83 | 7 967 ms |
  | the same two, directed | 83 | ~1.5 ms |

  Directed multi-hop patterns carry no penalty over chained one-hop at depth 2-3 on that
  fixture, so the cost is specific to undirectedness, and it compounds per element. `PathBuilder`
  therefore has no undirected variant at all; an undirected hop is two directed patterns.

- **The stricter rule is free.** Embedding scope into every element plans identically to the
  top-level form and runs in the same time, so there is no performance argument against the
  embed-in-every-element rule.
- **Scope must be addressable by graph variable** — see the section above. Do not fork the
  compiler.
- **sea_query models no PGQ.** Verified against sea_query 1.0.2. The AST, the renderer and the
  parameter binding are all new code; render through `StatementBuilder` so bound parameters are
  carried rather than formatted into SQL, as the CTE path does
  ([cte.rs:604-608](../../../../libs/toolkit-db/src/secure/cte.rs#L604-L608)). Note that
  `GRAPH_TABLE` *is* reachable through sea_query today without any new AST, via
  `TableRef::FunctionCall` + `Func::Custom` + `Expr::cust_*` — @vasylcf has a working traversal
  built that way. That is an escape hatch, not an alternative design: it carries the construct
  as an opaque string, so nothing scopes the elements. It falls under Level C, and the dylint
  rule must match the construct in `cust`-family arguments, not only in obvious raw-SQL sinks.
- **Identifiers must go through an escaping path, never `format!`.** Graph names, labels and
  element variables are identifiers in the emitted SQL. ADR-0001 records the precise
  mechanism and its two-branch gate (`Alias::new` always escapes; a `&'static str` used as an
  `Iden` escapes unless it passes `is_static_iden()`); the same reasoning must be applied here
  and pinned by hostile-name tests.
- **PostgreSQL 19 is pre-GA.** The image-tag half of this blocker is resolved: every fixture
  now starts through [`libs/test-containers`](../../../../libs/test-containers/src/lib.rs),
  which pins `POSTGRES_GRAPH_TAG` for the SQL/PGQ lane and exposes `postgres_graph()` to start
  it. Live coverage is `libs/toolkit-db/tests/pg/secure_graph.rs`, run by `make test-pgq` and
  by the CI `integration` job with `GEARS_TEST_PG_GRAPH_REQUIRED=1`, so an unavailable image
  fails that lane rather than letting the isolation suite pass vacuously; locally the default
  is still a skip while the tag is a pre-GA beta that Docker Hub may withdraw at GA.
- **Privileges give no isolation help — and no escalation risk.** Verbatim from the
  documentation: *"Access to the base relations underlying the `GRAPH_TABLE` clause is
  determined by the permissions of the user executing the query, rather than the property
  graph owner."* So a property graph is not a `SECURITY DEFINER`-style escalation path; it is
  also not a substitute for scope, because the platform connects with one role rather than one
  role per tenant.

### What this does not replace

PostgreSQL 19's initial implementation supports **fixed-depth patterns only**; variable-length
paths are deferred to a later release. A pattern must therefore spell out its hops, which means:

- **Unbounded or caller-configurable-depth traversal stays on ADR-0001's `recursive_cte`** or
  on a closure table. Notably, IRM's own design already assumes this shape — *"`parent_of`
  traversal reads the closure; `depends_on` and `attached_to` use bounded recursive traversal
  with a visited set"*
  ([DESIGN.md:263](../../../../gears/infrastructure-resource-manager/docs/DESIGN.md#L263)) —
  and a configurable-depth graph query is **not** expressible in PGQ v1.
- PGQ's value in v1 is *fixed-shape* pattern queries: a multi-hop, multi-entity join expressed
  once, declaratively, with the scope obligation discharged per element by the library instead
  of per join by the caller. That is the claim this ADR makes; the ADR does not claim traversal
  power over `recursive_cte`.

The three existing routes are unchanged and remain preferred where they apply: a closure table
for hot hierarchies, `InTenantSubtree` / `InGroupSubtree` for scope-level subtree filtering,
`recursive_cte` for rarely-walked or frequently-changing trees with a depth cap.

### Levels of strictness

- **Level A (safe, this decision)** — typed-element `SecureGraphSelect`, reachable only from a
  scoped select, only over `Scopable` entities that resolve a scope property, only over a
  generated property-graph declaration.
- **Level B (future, not implemented)** — label-addressed elements, admissible only if a
  future design can prove a unique entity per label. Recorded so it is not reinvented ad hoc;
  see Option B.
- **Level C (forbidden in user gears)** — a raw `GRAPH_TABLE` or `CREATE PROPERTY GRAPH`
  string in handlers/services/repos, or reaching a raw-SQL sink via
  `into_inner()`/`into_query()`. **This prohibition is binding immediately**, independently of
  when Level A ships, and it is syntactic, so it can be machine-checked by the same dylint rule
  ADR-0001 specifies for raw `WITH`.

### Consequences

**Positive:**

- Tenant isolation for graph queries is a compile-time property, by the same construction as
  for CTEs: no runtime scope comparison, no `Result` to handle, no check to remember.
- No new raw-SQL surface in gear code; the guardrail in
  [11_database_patterns.md:9](../../../toolkit_unified_system/11_database_patterns.md#L9) holds.
- One scope compiler serves selects, CTE bodies and graph elements, so a new `ScopeFilter`
  variant is implemented once.
- Label ↔ entity ↔ exposed-property consistency is generated rather than asserted by review.

**Negative:**

- **A gear that adopts PGQ becomes Postgres-only.** The `pgq` feature makes that explicit and
  compile-time, but it is a real reduction in deployment options for that gear, and it must be
  a deliberate product decision, not a convenience.
- The API is pinned to pre-GA syntax and must be re-validated at PostgreSQL 19 GA.
- A property graph is a new class of migration object: it must be created, replaced when any
  participating table's shape changes, and dropped on teardown, with a no-op on non-Postgres
  backends.
- An edge table must carry a scope column to participate, which is a modelling constraint on
  gears and rules out today's closure and membership tables.
- A second execution path (rendered `Statement` + `find_by_statement`) is extended rather than
  `Select<E>`'s, with the same divergence cost ADR-0001 accepted.
- The `toolkit-sea-orm-pgq` AST is code the platform now owns and must maintain against
  sea-orm/sea_query upgrades until sea_query models PGQ itself.

**Risks:**

- The **shape** of a pattern and the contents of `COLUMNS` are not compiler-verified. As with
  a `join_cte` predicate, this is a **correctness** risk, not an isolation risk: a wrong
  pattern can only under- or over-select rows, because no element ever contained another
  tenant's rows. Mitigate with tests and review.
- Subtree scopes rest on the correlated-sibling `FROM` shape, measured on PG19 beta 2 but not
  on GA. If it regresses, v1 falls back to rejecting those scopes and its usefulness narrows
  sharply for subtree-scoped tenants. That is a scope risk to the feature, not to isolation.
- The property-graph DDL is a schema object that can drift from the Rust declaration if it is
  ever hand-edited. Policy 3 removes the *source* of drift; a startup or migration-time check
  against `pg_catalog` would remove the rest and is recorded as a follow-up.
- Fixed-depth-only patterns invite pattern duplication (one variant per depth). Prefer
  `recursive_cte` over generating N patterns.

### Confirmation

**Not done — this ADR precedes the implementation.** Parts of the mechanism are nonetheless
already validated externally, on @vasylcf's probe branch against PG19 beta 2: a graph-addressed
scope predicate executes verbatim inside an element pattern and returns the owner's rows and
none for a foreign tenant; table addressing reproduces the current compiler's SQL exactly; and
the correlated-sibling shape works (see "Feasibility constraints"). That covers the mechanism,
not the guarantee — the tests below are what pin the guarantee. The test contract the
implementation PR must satisfy, following the philosophy that ADR-0001's tests established:

Assert on the **rendered SQL**, not on `Condition` values. A predicate that never reaches the
database would satisfy a `Debug`-form assertion and still leak — which is why
[cte_tests.rs](../../../../libs/toolkit-db/src/secure/cte_tests.rs) asserts on built
statements. And assert **per element**, not per statement: counting occurrences of `tenant_id`
in the whole query passes even when one element body is unscoped, because the column also
appears in `COLUMNS`. ADR-0001 records both traps as tests that passed against deliberately
broken code.

- `every_graph_element_embeds_scope` — the scope predicate is present inside **each** vertex
  body and **each** edge body, asserted separately per element.
- Deny-all scope → every element rejects. Allow-all → no accidental restriction is added.
- A caller predicate inside an element cannot remove that element's scope (scope is applied
  after the caller's, as `cte()` does).
- The `COLUMNS` projection cannot affect scope; an outer `WHERE` cannot substitute for element
  scope.
- An element table that resolves no scope property is rejected at build time — **not** turned
  into a silent deny-all traversal (Policy 2). This test is load-bearing: without it, the
  failure mode is an empty result set that looks like missing data.
- Graph-addressed `InGroup` / `InGroupSubtree` / `InTenantSubtree` return `Err`, never a
  dropped filter. Assert on the error *and* on the absence of a deny-all traversal, since the
  dropped-filter bug renders as a perfectly valid query.
- Table-addressed output is byte-identical to today's compiler for every `ScopeFilter` arm, so
  generalising the addressing cannot move the existing select path.
- A correlated-sibling scope does not multiply rows: a resource reachable through two group
  rows appears exactly once.
- Hostile graph name / label / element variable renders as exactly one quoted, escaped
  identifier, asserted **structurally** — a correctly escaped identifier still *contains* the
  dangerous substring, so substring searches prove nothing.
- Compile-fail test `graph_from_unscoped.rs` under `libs/toolkit-db/tests/ui/fail/`, alongside
  the existing `cte_from_unscoped.rs`; the harness already globs that directory
  ([tests/ui.rs](../../../../libs/toolkit-db/tests/ui.rs)).
- Live tests against a real PostgreSQL 19 server, in a new `tests/pg/` directory — the existing
  live CTE tests live in
  [tests/sqlite/secure_cte.rs](../../../../libs/toolkit-db/tests/sqlite/secure_cte.rs) and
  SQLite cannot host these. Cover, at minimum: cross-tenant edges are not traversed; an
  unscoped edge body would return foreign rows (i.e. the test can actually observe the hole);
  and the DDL generated for the graph matches what the query builder assumes.
- Observing an unscoped element requires care for the same reason ADR-0001 documents for CTEs:
  if the outer query re-filters on its own scope, it masks a leaking element. Pin the outer
  query to one row and make the element's contents visible in the row count.
- **Diff the renderer against a known-accepted oracle.** @vasylcf's `Func::Custom`-based
  traversal (branch `vasylcf/gears-rust:feature/graph-storage-pgq`) emits statements a real
  PG19 server accepts today. It is not a design this ADR adopts, but it is a cheap check that
  the new renderer produces syntax the server takes — worth diffing against before the live
  tests, since a renderer bug and a policy bug fail identically from the outside.

**Remaining:**

- ~~Settle the open subquery question against a PostgreSQL 19 server~~ — **done** on PG19
  beta 2, recorded under "Feasibility constraints". Re-confirm at GA, together with the
  correlated-sibling `FROM` shape the subtree scopes depend on.
- Codify the Level-C prohibition for `GRAPH_TABLE` in
  [11_database_patterns.md](../../../toolkit_unified_system/11_database_patterns.md), next to
  the CTE rules. **Not done.**
- Extend the dylint rule ADR-0001 specifies (cross-repo: `cargo-gears`,
  `crates/cargo-gears-lints`) to flag raw `GRAPH_TABLE` / `CREATE PROPERTY GRAPH` strings in
  gear crates. **Not done.**
- Register the new crate in all three places when it lands: the explicit `[workspace] members`
  list in [Cargo.toml](../../../../Cargo.toml) (a list, not a glob), a `[workspace.dependencies]`
  alias, and a `[[package]]` entry in [release-plz.toml](../../../../release-plz.toml) — whose
  own comment warns that an omission is not inert, since the crate would silently start
  producing its own changelog section and release page. Naming follows the workspace
  convention: package `cf-gears-toolkit-sea-orm-pgq`, `[lib] name = "toolkit_sea_orm_pgq"`,
  plus `README.md`, `description`, keywords/categories and `[lints] workspace = true`.
- Re-validate against PostgreSQL 19 GA (expected September/October 2026) before any gear ships
  a dependency on this API.
- A `pg_catalog` check that the deployed property graph matches the Rust declaration.

## Pros and Cons of the Options

### Option A: typed-element `SecureGraphSelect` from `SecureSelect<E, Scoped>`

- Good, because scope is embedded in every element pattern — isolation is a compile-time
  guarantee, not a review obligation.
- Good, because it reuses `build_scope_condition` and `ScopableEntity::resolve_property`
  unchanged in meaning; only column addressing is generalised.
- Good, because it adds no raw-SQL surface to gear code, and no `Statement`/`ConnectionTrait`
  escape from `toolkit-db`.
- Good, because the DDL and the query derive from one declaration, so label ↔ entity is
  compiler-checked.
- Neutral, because it needs a new AST crate — unavoidable, since sea_query models no PGQ.
- Bad, because the pattern shape and `COLUMNS` remain the caller's correctness burden.
- Bad, because it forces an anchor entity: the query begins from a scoped `SecureSelect<E>`
  even when the result is a pure graph projection. Partly redeemed — a pattern can correlate
  against `E`'s own columns, so the anchor doubles as the traversal's seed (see "Feasibility
  constraints").
- Bad, because an eligible edge table must carry a scope column, which excludes the platform's
  existing closure and membership tables.

### Option B: label-addressed elements with a runtime registry

- Good, because it reads closer to the SQL/PGQ language and would allow patterns over labels
  the DBA defined.
- Bad, because a label may map to several element tables, so the entity — and therefore the
  scope condition — is not uniquely determined.
- Bad, because a registry lookup is exactly the runtime check that ADR-0001's structural design
  exists to avoid, and it would have to fail closed on a miss, reintroducing the silent
  empty-result failure mode.
- Deferred as Level B, not implemented.

### Option C: raw `GRAPH_TABLE` / `CREATE PROPERTY GRAPH` strings in gear code

- Good, because maximally flexible, and immediately able to use syntax the builder does not
  model.
- Bad, because it discards the typestate guarantee entirely and violates the no-plain-SQL rule
  ([11_database_patterns.md:9](../../../toolkit_unified_system/11_database_patterns.md#L9)).
- Bad, because nothing then guarantees that any element pattern is scoped — and the outer
  `WHERE` looks like it does.
- Rejected for user-gear code, effective immediately. Migrations and `toolkit-db` internals are
  a different layer, as in ADR-0001.

### Option D: decline SQL/PGQ; keep `recursive_cte` and closure tables

- Good, because it adds no crate, no feature flag, no pre-GA dependency, and no Postgres-only
  API.
- Good, because with variable-length paths absent from PostgreSQL 19, it covers **more**
  traversal shapes today than PGQ does — this is the honest baseline, not a straw man.
- Bad, because a fixed-shape multi-entity traversal is expressed as a chain of joins whose
  scope obligation the caller discharges per join, which is precisely the kind of per-site
  obligation this ORM exists to remove.
- Bad, because declining now means deciding the policy later, under adoption pressure, which is
  the failure mode ADR-0001 was written to avoid.
- Not chosen, but note that Option D remains the *implementation* answer for anything requiring
  variable or caller-configured depth; the two coexist.

## More Information

- CTE policy this ADR extends: [ADR-0001](./0001-secure-cte-policy.md)
- CTE implementation, the structural pattern reused here:
  [libs/toolkit-db/src/secure/cte.rs](../../../../libs/toolkit-db/src/secure/cte.rs)
- Scope condition builder (the compiler to generalise, not fork):
  [libs/toolkit-db/src/secure/cond.rs](../../../../libs/toolkit-db/src/secure/cond.rs)
- Typestate and execution:
  [libs/toolkit-db/src/secure/select.rs](../../../../libs/toolkit-db/src/secure/select.rs),
  [libs/toolkit-db/src/secure/runner.rs](../../../../libs/toolkit-db/src/secure/runner.rs)
- Entity scope contract:
  [libs/toolkit-db/src/secure/entity_traits.rs](../../../../libs/toolkit-db/src/secure/entity_traits.rs)
- Raw-SQL policy and gear-facing rules:
  [11_database_patterns.md](../../../toolkit_unified_system/11_database_patterns.md)
- Closure vs recursive traversal rationale:
  [docs/arch/authorization/DESIGN.md](../../authorization/DESIGN.md)
- First intended consumer:
  [IRM DESIGN.md](../../../../gears/infrastructure-resource-manager/docs/DESIGN.md),
  [IRM PRD.md](../../../../gears/infrastructure-resource-manager/docs/PRD.md)
- ADR template & checklist: [docs/checklists/ADR.md](../../../checklists/ADR.md)
- PostgreSQL 19 (beta) documentation:
  [7.9. Graph Queries](https://www.postgresql.org/docs/19/queries-graph.html),
  [5.15. Property Graphs](https://www.postgresql.org/docs/19/ddl-property-graphs.html),
  [CREATE PROPERTY GRAPH](https://www.postgresql.org/docs/19/sql-create-property-graph.html)
- SQL/PGQ is ISO/IEC 9075-16; `CREATE PROPERTY GRAPH` conforms to it per the PostgreSQL
  documentation's Compatibility note.
