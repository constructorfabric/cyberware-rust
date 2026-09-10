//! Secure property-graph declarations.
//!
//! One Rust declaration is the source of **both** the `CREATE PROPERTY GRAPH`
//! DDL a migration executes and the labels a `MATCH` addresses
//! (`docs/arch/secure-orm/ADR/0002`, Policy 3). Nothing else keeps those two
//! sides in step, and the failure when they drift is quiet: a column left out of
//! the DDL's `PROPERTIES` list does not make the graph invalid, it just makes
//! that column invisible to the pattern language — so a scope column left out
//! makes the pattern unscopable while everything still parses.
//!
//! # What the declaration guarantees
//!
//! * **Every element exposes its scope columns as properties.** Derived from
//!   [`ScopableEntity::scope_columns`], not from a list the caller repeats, so
//!   the precondition for Policy 2 cannot be forgotten.
//! * **One label per element table.** Sharing a label across tables is legal
//!   SQL/PGQ and unsafe here: security is decided per entity, so a shared label
//!   would have several security mappings and the builder would have to pick
//!   one (Policy 1). The declaration refuses it.
//! * **Every element resolves at least one scope column.** An entity that
//!   resolves none compiles to `WHERE false` under any constrained scope — safe,
//!   and indistinguishable from a legitimate deny-all, which is why it is
//!   refused here rather than detected later (Policy 2).
//!
//! # Membership is a type-level fact
//!
//! [`VertexOf`] and [`EdgeOf`] are what make `label ↔ entity` compiler-checked
//! rather than a convention held in two files. A builder that accepts
//! `J: VertexOf<G>` cannot be handed an entity that is not part of `G`, and
//! cannot be handed an edge where a vertex belongs. The query builder
//! additionally checks the pattern against [`PropertyGraph::declaration`] at
//! build time, so an entity that implements the marker trait but was never
//! registered in the declaration fails here, with a message, rather than at the
//! server.

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::sync::Arc;

use sea_orm::sea_query::{Alias, IntoIden, Query, SelectStatement, TableRef};
use sea_orm::{Condition, EntityTrait, FromQueryResult, QueryTrait, StatementBuilder};
use toolkit_sea_orm_pgq::{EdgeTable, EndpointRef, PropertyGraph as GraphDdl, VertexTable};

use crate::secure::cond::{ColumnAddress, SiblingSupport, build_scope_predicate};
use crate::secure::select::{Scoped, SecureSelect};
use crate::secure::{AccessScope, DBRunner, DBRunnerInternal, ScopableEntity, ScopeError};

/// A property graph the platform declares.
///
/// Implement it — or derive it — on a marker type, never on an entity: a graph
/// is a schema object spanning several tables.
pub trait PropertyGraph {
    /// Name of the graph object in the database.
    const GRAPH_NAME: &'static str;

    /// The declaration, from which both the DDL and the query AST are built.
    ///
    /// # Errors
    /// Returns [`ScopeError::Invalid`] when the declaration violates Policy 1
    /// or Policy 2.
    fn declaration() -> Result<GraphDeclaration, ScopeError>;
}

/// An entity that participates in `G` as a vertex.
pub trait VertexOf<G: PropertyGraph>: ScopableEntity + EntityTrait {
    /// Label the pattern language addresses this element by.
    const LABEL: &'static str;
}

/// An entity that participates in `G` as an edge.
pub trait EdgeOf<G: PropertyGraph>: ScopableEntity + EntityTrait {
    /// Label the pattern language addresses this element by.
    const LABEL: &'static str;
}

/// One endpoint of an edge: which columns point at which vertex table.
#[derive(Clone, Debug)]
pub struct Endpoint {
    /// Columns on the edge table.
    pub key: Vec<String>,
    /// Vertex table they reference.
    pub table: String,
    /// Columns on that vertex table.
    pub references: Vec<String>,
}

/// A graph declaration under construction.
///
/// Elements are added by entity type, so the label and the scope columns come
/// from the entity rather than from strings the caller repeats.
#[derive(Debug, Default)]
pub struct GraphDeclaration {
    name: String,
    vertices: Vec<VertexTable>,
    edges: Vec<EdgeTable>,
    labels: BTreeSet<String>,
}

impl GraphDeclaration {
    /// Start a declaration for `G`.
    #[must_use]
    pub fn new<G: PropertyGraph>() -> Self {
        Self {
            name: G::GRAPH_NAME.to_owned(),
            ..Self::default()
        }
    }

    /// Register `J` as a vertex, keyed on `key`.
    ///
    /// # Errors
    /// Returns [`ScopeError::Invalid`] when `J` resolves no scope column
    /// (Policy 2) or when its label is already taken (Policy 1), and
    /// [`ScopeError::GraphSyntax`] when the element cannot be rendered (e.g. an empty
    /// key).
    pub fn vertex<G, J>(mut self, key: &[&str]) -> Result<Self, ScopeError>
    where
        G: PropertyGraph,
        J: VertexOf<G>,
    {
        let element = self.element::<J>(J::LABEL, key)?;
        self.vertices.push(element);
        Ok(self)
    }

    /// Register `J` as an edge between two vertex tables.
    ///
    /// # Errors
    /// As [`Self::vertex`], plus an endpoint that names no columns or whose
    /// key and referenced columns differ in arity.
    pub fn edge<G, J>(
        mut self,
        key: &[&str],
        source: Endpoint,
        destination: Endpoint,
    ) -> Result<Self, ScopeError>
    where
        G: PropertyGraph,
        J: EdgeOf<G>,
    {
        let element = self.element::<J>(J::LABEL, key)?;
        self.edges.push(EdgeTable::new(
            element,
            into_endpoint(source)?,
            into_endpoint(destination)?,
        ));
        Ok(self)
    }

    /// Build the element common to vertices and edges, enforcing both policies.
    fn element<J>(&mut self, label: &str, key: &[&str]) -> Result<VertexTable, ScopeError>
    where
        J: ScopableEntity + EntityTrait,
    {
        // Policy 2, checked here rather than after compiling a condition: an
        // entity that resolves no scope column compiles to exactly the same
        // predicate under a real scope as a legitimate deny-all does, so by the
        // time there is a `Condition` the two are indistinguishable.
        let scope_columns = J::scope_columns();
        if scope_columns.is_empty() {
            return Err(ScopeError::Invalid(
                "a property-graph element must resolve at least one scope column; \
                 an element that resolves none would traverse as a silent deny-all",
            ));
        }

        // Policy 1: one label per element table. Legal SQL/PGQ, unsafe here.
        if !self.labels.insert(label.to_owned()) {
            return Err(ScopeError::Invalid(
                "two elements of one property graph share a label; security is decided \
                 per entity, so a shared label would have several security mappings",
            ));
        }

        // The properties list is the union of the key columns and every scope
        // column. The key is there so an endpoint reference can resolve; the
        // scope columns are there so a pattern can be scoped at all.
        let mut properties: Vec<String> = key.iter().map(|c| (*c).to_owned()).collect();
        for column in scope_columns {
            let name = sea_orm::IdenStatic::as_str(&column).to_owned();
            if !properties.contains(&name) {
                properties.push(name);
            }
        }

        VertexTable::new(
            J::default().table_name(),
            key.iter().map(|c| (*c).to_owned()).collect(),
            label,
            properties,
        )
        .map_err(syntax_error)
    }

    /// Render the `CREATE PROPERTY GRAPH` statement.
    ///
    /// # Errors
    /// Returns [`ScopeError::GraphSyntax`] when the declaration cannot be rendered.
    pub fn create_statement(&self) -> Result<String, ScopeError> {
        self.ddl().create_statement().map_err(syntax_error)
    }

    /// Render the matching `DROP PROPERTY GRAPH IF EXISTS`.
    ///
    /// # Errors
    /// As [`Self::create_statement`].
    pub fn drop_statement(&self) -> Result<String, ScopeError> {
        self.ddl().drop_statement().map_err(syntax_error)
    }

    fn ddl(&self) -> GraphDdl {
        GraphDdl::new(self.name.clone(), self.vertices.clone(), self.edges.clone())
    }

    /// Labels declared so far, for tests and diagnostics.
    pub fn labels(&self) -> impl Iterator<Item = &str> {
        self.labels.iter().map(String::as_str)
    }

    /// Properties exposed for `label`, if it is declared.
    #[must_use]
    pub fn properties_of(&self, label: &str) -> Option<&[String]> {
        self.vertices
            .iter()
            .chain(self.edges.iter().map(EdgeTable::element))
            .find(|element| element.label() == label)
            .map(VertexTable::properties)
    }

    /// Table backing the element declared as `label`, if it is declared.
    #[must_use]
    pub fn table_of(&self, label: &str) -> Option<&str> {
        self.vertices
            .iter()
            .chain(self.edges.iter().map(EdgeTable::element))
            .find(|element| element.label() == label)
            .map(VertexTable::table)
    }
}

fn into_endpoint(endpoint: Endpoint) -> Result<EndpointRef, ScopeError> {
    EndpointRef::new(endpoint.key, endpoint.table, endpoint.references).map_err(syntax_error)
}

/// Carry a syntax-layer refusal into this crate's error type.
///
/// A plain function rather than a `From` impl, and the payload is the rendered
/// message: the syntax crate's error type must not appear in `ScopeError`, or
/// every gear on every backend would link a `PostgreSQL` 19 crate to name the
/// variant. The refusal's own wording is what a caller can act on.
// By value because it is the `map_err` adapter at every syntax-layer call
// site; a by-reference signature would cost a closure at each of them.
#[allow(clippy::needless_pass_by_value)]
fn syntax_error(error: toolkit_sea_orm_pgq::PgqError) -> ScopeError {
    ScopeError::GraphSyntax(error.to_string())
}

// ───────────────────────── the secure graph query ─────────────────────────

/// A graph query whose every element carries the caller's scope.
///
/// Reachable only from a scoped select, mirroring `with_ctes`: the scope the
/// elements inherit is the one the outer query already carries, so a
/// differently-scoped element cannot be constructed.
///
/// Everything the caller registers is recorded and assembled in `build()`,
/// because one decision can only be taken once the whole pattern is known:
/// whether the anchor query stays in the `FROM` at all (see
/// [`SecureSelect::with_graph`]).
pub struct SecureGraphSelect<E: EntityTrait, G: PropertyGraph> {
    /// The scoped entity query `with_graph` was called on, scope `WHERE`
    /// already embedded. Placed in the `FROM` only when the pattern carries a
    /// caller predicate that can correlate against it; otherwise dropped, so an
    /// uncorrelated anchor cannot multiply rows.
    anchor: SelectStatement,
    /// Seeds every element predicate. This is what makes same-scope structural.
    scope: Arc<AccessScope>,
    pattern: Option<PathState>,
    columns: Vec<toolkit_sea_orm_pgq::ProjectedColumn>,
    filters: Vec<Condition>,
    limit: Option<u64>,
    distinct: bool,
    /// Relations the element predicates correlate against, deduplicated by
    /// alias so one relation referenced by several elements is placed once.
    siblings: Vec<SiblingPlacement>,
    /// The first refusal raised inside a builder closure.
    ///
    /// The closures cannot return `Result`, so a refusal is carried here and
    /// surfaced at execution. What matters is that it is not silent: the query
    /// never runs with a missing predicate.
    error: Option<ScopeError>,
    graph_alias: &'static str,
    _marker: PhantomData<(E, G)>,
}

/// A correlated relation and the alias it is placed under.
#[derive(Clone)]
struct SiblingPlacement {
    alias: String,
    query: SelectStatement,
}

/// One pattern element under construction.
///
/// The caller's predicates and the scope condition are kept apart until the
/// element is finalized, so the rendered body always reads caller predicate
/// first, scope after — scope is applied *on top of* whatever the caller wrote,
/// and `Element::and_where` only ever narrows, so a predicate cannot filter the
/// scope back off.
struct PendingElement {
    variable: &'static str,
    label: &'static str,
    /// The entity's table, so the build can check the label resolves to *this*
    /// entity in the declaration and not to another one sharing the label.
    table: String,
    caller: Vec<Condition>,
    scope: Condition,
}

impl PendingElement {
    fn finalize(self) -> toolkit_sea_orm_pgq::Element {
        let mut element = toolkit_sea_orm_pgq::Element::new(self.variable, self.label);
        for condition in self.caller {
            element = element.and_where(condition);
        }
        element.and_where(self.scope)
    }
}

/// Pattern under construction, with the pending edge a hop needs.
#[derive(Default)]
struct PathState {
    head: Option<PendingElement>,
    hops: Vec<(
        PendingElement,
        toolkit_sea_orm_pgq::Direction,
        PendingElement,
    )>,
    pending_edge: Option<(PendingElement, toolkit_sea_orm_pgq::Direction)>,
    /// Whether the caller asked to keep the anchor in the `FROM`
    /// ([`PathBuilder::correlate_with_anchor`]). Not inferred from the
    /// predicates: a `where_` that never names the anchor would keep it as an
    /// uncorrelated cross join.
    anchor_correlated: bool,
}

impl PathState {
    /// The element a `where_` call attaches to: the most recently added one.
    fn last_element_mut(&mut self) -> Option<&mut PendingElement> {
        if let Some((edge, _)) = self.pending_edge.as_mut() {
            return Some(edge);
        }
        if let Some((_, _, target)) = self.hops.last_mut() {
            return Some(target);
        }
        self.head.as_mut()
    }

    /// Every element, head first, in written order.
    fn elements(&self) -> impl Iterator<Item = &PendingElement> {
        self.head.iter().chain(
            self.hops
                .iter()
                .flat_map(|(edge, _, target)| [edge, target]),
        )
    }
}

impl<E> SecureSelect<E, Scoped>
where
    E: EntityTrait,
{
    /// Begin a graph query over `G`.
    ///
    /// Every pattern element registered on the result is scoped with **this**
    /// query's `AccessScope`.
    ///
    /// The query itself becomes the *anchor*: it stays in the `FROM` — already
    /// scoped — when the pattern asks for it with
    /// [`PathBuilder::correlate_with_anchor`] and correlates against it through
    /// an element predicate ([`PathBuilder::where_`]), which is the "start from
    /// these rows and walk out one hop" shape. Otherwise the anchor is dropped
    /// from the `FROM` rather than left as an uncorrelated cross join that
    /// multiplies every match by the anchor's row count.
    #[must_use]
    pub fn with_graph<G: PropertyGraph>(self) -> SecureGraphSelect<E, G> {
        let scope = self.scope_arc();
        SecureGraphSelect {
            anchor: QueryTrait::into_query(self.into_inner()),
            scope,
            pattern: None,
            columns: Vec::new(),
            filters: Vec::new(),
            limit: None,
            distinct: false,
            siblings: Vec::new(),
            error: None,
            graph_alias: "cf_graph",
            _marker: PhantomData,
        }
    }
}

/// Builds the pattern, attaching scope to every element as it is added.
///
/// Elements are addressed by entity type, never by label: security is decided
/// per entity, and one label may span several element tables, so a
/// label-addressed element would have several security mappings (Policy 1).
pub struct PathBuilder<G: PropertyGraph> {
    scope: Arc<AccessScope>,
    state: PathState,
    siblings: Vec<SiblingPlacement>,
    error: Option<ScopeError>,
    _marker: PhantomData<G>,
}

impl<G: PropertyGraph> PathBuilder<G> {
    /// Start the pattern at a vertex.
    #[must_use]
    pub fn vertex<J>(mut self, variable: &'static str) -> Self
    where
        J: VertexOf<G>,
        J::Column: sea_orm::ColumnTrait + Copy,
    {
        match self.scoped_element::<J>(variable, J::LABEL) {
            Ok(element) => {
                if self.state.head.is_some() {
                    self.fail(ScopeError::Invalid(
                        "a pattern has one head vertex; reach the others with edge_to/edge_from",
                    ));
                } else {
                    self.state.head = Some(element);
                }
            }
            Err(e) => self.fail(e),
        }
        self
    }

    /// Follow an edge away from the current vertex.
    #[must_use]
    pub fn edge_to<J>(mut self, variable: &'static str) -> Self
    where
        J: EdgeOf<G>,
        J::Column: sea_orm::ColumnTrait + Copy,
    {
        self.edge::<J>(variable, toolkit_sea_orm_pgq::Direction::Outgoing);
        self
    }

    /// Follow an edge into the current vertex.
    #[must_use]
    pub fn edge_from<J>(mut self, variable: &'static str) -> Self
    where
        J: EdgeOf<G>,
        J::Column: sea_orm::ColumnTrait + Copy,
    {
        self.edge::<J>(variable, toolkit_sea_orm_pgq::Direction::Incoming);
        self
    }

    /// Complete the hop at a vertex.
    #[must_use]
    pub fn to<J>(mut self, variable: &'static str) -> Self
    where
        J: VertexOf<G>,
        J::Column: sea_orm::ColumnTrait + Copy,
    {
        let Some((edge, direction)) = self.state.pending_edge.take() else {
            self.fail(ScopeError::Invalid(
                "to() completes a hop; call edge_to() or edge_from() first",
            ));
            return self;
        };
        match self.scoped_element::<J>(variable, J::LABEL) {
            Ok(target) => self.state.hops.push((edge, direction, target)),
            Err(e) => self.fail(e),
        }
        self
    }

    /// Narrow the element added last with the caller's own predicate.
    ///
    /// The predicate is written inside the element's body, **before** the scope
    /// condition — scope is applied on top of it and cannot be filtered back
    /// off. Columns are addressed by the element's variable
    /// (`Expr::col(("a", "id"))`). A predicate comparing an element's columns
    /// against the anchor entity's own columns is how a pattern correlates with
    /// the anchor query — pair it with [`Self::correlate_with_anchor`], which
    /// is what keeps the anchor in the `FROM`.
    #[must_use]
    pub fn where_(mut self, condition: Condition) -> Self {
        if condition.is_empty() {
            self.fail(ScopeError::Invalid(
                "an empty element predicate constrains nothing; \
                 drop the where_() call or add a filter to the condition",
            ));
            return self;
        }
        if let Some(element) = self.state.last_element_mut() {
            element.caller.push(condition);
        } else {
            self.fail(ScopeError::Invalid(
                "where_() narrows the last added element; add a vertex or an edge first",
            ));
        }
        self
    }

    /// Keep the anchor query in the `FROM`, because a predicate on this pattern
    /// correlates against it.
    ///
    /// The anchor — the scoped entity query `with_graph` was called on — is
    /// dropped by default: nothing but a caller predicate can reference it, and
    /// an unreferenced comma-joined relation multiplies every match by its row
    /// count. Whether a predicate *does* reference it is not inferred from the
    /// predicate (a `where_` that never names the anchor would reproduce that
    /// cross join), so keeping the anchor is this explicit decision. Pair it
    /// with a [`Self::where_`] comparing an element's columns against the anchor
    /// entity's own columns — the "start from these rows, walk out one hop"
    /// shape.
    #[must_use]
    pub fn correlate_with_anchor(mut self) -> Self {
        self.state.anchor_correlated = true;
        self
    }

    fn edge<J>(&mut self, variable: &'static str, direction: toolkit_sea_orm_pgq::Direction)
    where
        J: EdgeOf<G>,
        J::Column: sea_orm::ColumnTrait + Copy,
    {
        if self.state.head.is_none() {
            self.fail(ScopeError::Invalid(
                "a pattern starts at a vertex; call vertex() before an edge",
            ));
            return;
        }
        if self.state.pending_edge.is_some() {
            self.fail(ScopeError::Invalid(
                "two edges in a row; complete the hop with to() first",
            ));
            return;
        }
        match self.scoped_element::<J>(variable, J::LABEL) {
            Ok(element) => self.state.pending_edge = Some((element, direction)),
            Err(e) => self.fail(e),
        }
    }

    /// Build one element with its scope predicate attached.
    ///
    /// Policy 2 first: an entity that resolves no scope column is refused here,
    /// because after a condition exists it is indistinguishable from a
    /// legitimate deny-all. The same policy is then checked against the *live*
    /// scope: an entity may well declare scope columns and still resolve none
    /// of the properties this particular scope addresses, which would compile
    /// to the very same silent deny-all.
    fn scoped_element<J>(
        &mut self,
        variable: &'static str,
        label: &'static str,
    ) -> Result<PendingElement, ScopeError>
    where
        J: ScopableEntity + EntityTrait,
        J::Column: sea_orm::ColumnTrait + Copy,
    {
        if J::scope_columns().is_empty() {
            return Err(ScopeError::Invalid(
                "a graph element must resolve at least one scope column; \
                 an element that resolves none would traverse as a silent deny-all",
            ));
        }
        self.require_scope_resolves::<J>(variable)?;

        let predicate = build_scope_predicate::<J>(
            &self.scope,
            ColumnAddress::GraphElement {
                var: variable,
                siblings: SiblingSupport::Allowed,
            },
        )?;
        let (condition, siblings) = predicate.into_parts();

        // A scope that is neither unconstrained nor deny-all must leave a real
        // predicate on the element. An empty condition here would be dropped by
        // `Element::and_where`, and the element would traverse every tenant's
        // rows under a scope that never said allow-all — so "no predicate" is
        // reachable only from an explicitly unconstrained scope.
        if condition.is_empty() && !self.scope.is_unconstrained() {
            return Err(ScopeError::Invalid(
                "the scope compiled to an empty predicate for a graph element; \
                 only an explicitly unconstrained scope may leave an element \
                 without a predicate",
            ));
        }

        for sibling in siblings {
            // Deduplicated by alias: the same scope compiled for two elements
            // names the same relation, and placing it twice would turn the
            // correlation into a cross join and multiply rows.
            if !self.siblings.iter().any(|p| p.alias == sibling.alias) {
                self.siblings.push(SiblingPlacement {
                    alias: sibling.alias,
                    query: sibling.query,
                });
            }
        }

        Ok(PendingElement {
            variable,
            label,
            table: J::default().table_name().to_owned(),
            caller: Vec::new(),
            scope: condition,
        })
    }

    /// Refuse an element on which no constraint of the live scope resolves.
    ///
    /// `build_scope_predicate` drops a constraint whose property does not
    /// resolve — the fail-closed rule for OR-ed alternatives — and falls to
    /// deny-all when *every* constraint dropped. On the graph path that
    /// deny-all is indistinguishable from missing data, so it is refused here
    /// by name instead (Policy 2, the same reasoning as the declaration-time
    /// gate, applied to the scope actually in force).
    fn require_scope_resolves<J>(&self, variable: &'static str) -> Result<(), ScopeError>
    where
        J: ScopableEntity + EntityTrait,
    {
        if self.scope.is_unconstrained() || self.scope.is_deny_all() {
            return Ok(());
        }
        let mut first_unresolved: Option<&str> = None;
        for constraint in self.scope.constraints() {
            match constraint
                .filters()
                .iter()
                .find(|filter| J::resolve_property(filter.property()).is_none())
            {
                // Every filter of this constraint resolves: the scope is
                // servable on this element.
                None => return Ok(()),
                Some(filter) => {
                    first_unresolved.get_or_insert(filter.property());
                }
            }
        }
        Err(ScopeError::UnresolvedScopeProperty {
            element: variable,
            property: first_unresolved.unwrap_or_default().to_owned(),
        })
    }

    fn fail(&mut self, error: ScopeError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }
}

impl<E, G> SecureGraphSelect<E, G>
where
    E: EntityTrait,
    G: PropertyGraph,
{
    /// Describe the pattern.
    ///
    /// Scope is attached to every element as it is added, so a predicate the
    /// closure adds afterwards narrows the element rather than replacing what
    /// the library put there. Callable once: a second pattern would silently
    /// replace the first while keeping the relations it placed, so it is
    /// refused instead.
    #[must_use]
    pub fn match_path(mut self, f: impl FnOnce(PathBuilder<G>) -> PathBuilder<G>) -> Self {
        if self.pattern.is_some() {
            self.fail(ScopeError::Invalid(
                "match_path() was called twice; a graph query has one pattern",
            ));
            return self;
        }

        let builder = f(PathBuilder {
            scope: Arc::clone(&self.scope),
            state: PathState::default(),
            siblings: Vec::new(),
            error: None,
            _marker: PhantomData,
        });

        if let Some(error) = builder.error {
            self.fail(error);
            return self;
        }
        if builder.state.pending_edge.is_some() {
            self.fail(ScopeError::Invalid(
                "the pattern ends on an edge; complete the hop with to()",
            ));
            return self;
        }

        for sibling in builder.siblings {
            if !self.siblings.iter().any(|p| p.alias == sibling.alias) {
                self.siblings.push(sibling);
            }
        }
        self.pattern = Some(builder.state);
        self
    }

    /// Project a graph property into the result.
    #[must_use]
    pub fn column(
        mut self,
        variable: &'static str,
        property: &'static str,
        alias: &'static str,
    ) -> Self {
        self.columns.push(toolkit_sea_orm_pgq::ProjectedColumn::new(
            variable, property, alias,
        ));
        self
    }

    /// Narrow the outer query, on top of the scope the elements already carry.
    #[must_use]
    pub fn filter(mut self, filter: Condition) -> Self {
        self.filters.push(filter);
        self
    }

    /// Cap the number of rows.
    #[must_use]
    pub fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Deduplicate the outer result.
    #[must_use]
    pub fn distinct(mut self) -> Self {
        self.distinct = true;
        self
    }

    fn fail(&mut self, error: ScopeError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    /// Check the pattern against the graph's declaration (Policy 3).
    ///
    /// `VertexOf`/`EdgeOf` tie an entity to the graph *type*, but nothing ties
    /// an `impl` to the declaration — an entity could be patterned into a graph
    /// it was never registered in, and the mistake would surface as a server
    /// error naming no Rust construct. Checked here instead: every label must
    /// be declared *for the entity the pattern uses* (two entities may share a
    /// label; only the registered one is what the label means), and every
    /// projected property must be in the element's
    /// `PROPERTIES` list, where a missing entry is otherwise *silently*
    /// unfilterable.
    fn check_declaration(
        state: &PathState,
        columns: &[toolkit_sea_orm_pgq::ProjectedColumn],
    ) -> Result<(), ScopeError> {
        let declaration = G::declaration()?;

        let mut label_of: BTreeMap<&str, &str> = BTreeMap::new();
        for element in state.elements() {
            match declaration.table_of(element.label) {
                None => {
                    return Err(ScopeError::Invalid(
                        "the pattern addresses a label the graph declaration does not \
                         declare; register the entity in the declaration",
                    ));
                }
                // The label alone is not membership: two entities can both
                // implement the marker trait under one label, and only the
                // registered one is what the label resolves to on the server.
                // Scope compiled from the other entity's columns would then
                // guard a traversal of a table it was never written for.
                Some(table) if table != element.table => {
                    return Err(ScopeError::Invalid(
                        "the pattern binds a label to an entity other than the one the \
                         graph declaration registers under it; the scope predicate would \
                         be compiled for one table while the label resolves to another",
                    ));
                }
                Some(_) => {}
            }
            label_of.insert(element.variable, element.label);
        }

        for column in columns {
            let Some(label) = label_of.get(column.variable()) else {
                return Err(ScopeError::Invalid(
                    "a projected column names a variable the pattern does not bind",
                ));
            };
            let exposed = declaration
                .properties_of(label)
                .is_some_and(|properties| properties.iter().any(|p| p == column.property()));
            if !exposed {
                return Err(ScopeError::Invalid(
                    "a projected property is not in the element's PROPERTIES list; \
                     a column absent from that list is invisible to MATCH; expose \
                     it through the element's key or scope columns",
                ));
            }
        }
        Ok(())
    }

    /// Assemble the statement: the anchor (if the pattern correlates against
    /// it), the correlated siblings, and the pattern, in one `FROM`.
    fn build(self) -> Result<SelectStatement, ScopeError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let Some(state) = self.pattern else {
            return Err(ScopeError::Invalid(
                "a graph query needs a pattern; call match_path()",
            ));
        };

        Self::check_declaration(&state, &self.columns)?;

        // The anchor stays only on the caller's explicit say-so. Nothing but a
        // caller predicate can reference it, and an unreferenced comma-joined
        // relation is a pure row multiplier: `column()` selects graph columns
        // only, so each match would return once per anchor row. The scope the
        // anchor carried is not lost — every element body embeds the same scope.
        let mut outer = if state.anchor_correlated {
            self.anchor
        } else {
            Query::select()
        };
        // The graph query's result is the projected graph columns; whatever the
        // anchor selected is replaced rather than appended to.
        outer.clear_selects();

        let Some(head) = state.head else {
            return Err(ScopeError::Invalid(
                "a graph query needs a head vertex; call vertex()",
            ));
        };
        let mut pattern = toolkit_sea_orm_pgq::GraphPattern::new(head.finalize());
        for (edge, direction, target) in state.hops {
            pattern = pattern.hop(edge.finalize(), direction, target.finalize());
        }

        let mut table = toolkit_sea_orm_pgq::GraphTable::new(G::GRAPH_NAME, pattern);
        for column in &self.columns {
            outer.expr_as(
                sea_orm::sea_query::Expr::col((
                    Alias::new(self.graph_alias),
                    Alias::new(column.alias()),
                )),
                Alias::new(column.alias()),
            );
            table = table.column(column.clone());
        }

        // Siblings first, then the pattern: a comma join, which PostgreSQL
        // treats as an implicit lateral, so the pattern's correlated references
        // resolve. `LATERAL` itself is refused before `GRAPH_TABLE`.
        for sibling in self.siblings {
            outer.from(TableRef::SubQuery(
                Box::new(sibling.query),
                Alias::new(sibling.alias.as_str()).into_iden(),
            ));
        }
        outer.from(
            table
                .into_table_ref(self.graph_alias)
                .map_err(syntax_error)?,
        );

        for filter in self.filters {
            outer.cond_where(filter);
        }
        if let Some(limit) = self.limit {
            outer.limit(limit);
        }
        if self.distinct {
            outer.distinct();
        }

        Ok(outer)
    }

    /// Render the statement for `backend` without executing it.
    ///
    /// Exists because this crate has no mock database: it is the only way for a
    /// test to assert on the emitted SQL. Crate-private — `sea_orm::Statement`
    /// and `DbBackend` must not appear in this crate's public surface, exactly
    /// as on the CTE path (see the `runner` module docs).
    ///
    /// # Errors
    /// Returns [`ScopeError::Invalid`] for a pattern that cannot be built.
    #[cfg(test)]
    pub(crate) fn build_statement(
        self,
        backend: sea_orm::DbBackend,
    ) -> Result<sea_orm::Statement, ScopeError> {
        self.into_statement(backend)
    }

    /// Render the statement for execution.
    fn into_statement(self, backend: sea_orm::DbBackend) -> Result<sea_orm::Statement, ScopeError> {
        let query = self.build()?;
        Ok(StatementBuilder::build(&query, &backend))
    }

    /// Execute and deserialize into `T`.
    ///
    /// # Errors
    /// Returns [`ScopeError::Invalid`] for a pattern that cannot be built, or
    /// [`ScopeError::Db`] when the query fails.
    pub async fn all_as<T>(self, runner: &impl DBRunner) -> Result<Vec<T>, ScopeError>
    where
        T: FromQueryResult + Send + Sync,
    {
        let exec = DBRunnerInternal::as_seaorm(runner);
        let stmt = self.into_statement(exec.backend())?;
        Ok(match exec {
            crate::secure::SeaOrmRunner::Conn(db) => T::find_by_statement(stmt).all(db).await?,
            crate::secure::SeaOrmRunner::Tx(tx) => T::find_by_statement(stmt).all(tx).await?,
        })
    }
}
