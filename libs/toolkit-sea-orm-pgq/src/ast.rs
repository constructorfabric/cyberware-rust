//! The typed `GRAPH_TABLE` AST.
//!
//! Structure only: what a pattern *is*. How it becomes SQL lives in the
//! `render` module, which is also where the rendering entry points on
//! [`GraphTable`] are implemented. Named in prose rather than as a doc link on
//! purpose: this module must not reference `render` at all, or the two depend
//! on each other again and the one-way AST-to-renderer edge is lost.
//!
//! Fields the renderer reads are `pub(crate)`. That is not a widening: these
//! types used to sit in the crate root, where "private" already meant
//! crate-visible to every descendant module. The marker makes the reach
//! explicit rather than incidental.
//!
//! It also cannot be relaxed to `pub`. Every type here except [`Hop`] is
//! re-exported from the crate root, so `pub` fields would become part of the
//! public API and let a caller build a [`GraphTable`] around the builders whose
//! invariants the renderer checks.

use sea_orm::Condition;

/// Which way an edge is followed.
///
/// There is no undirected variant; see the crate documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// `(a)-[e]->(b)` — from the preceding vertex to the following one.
    Outgoing,
    /// `(a)<-[e]-(b)` — into the preceding vertex from the following one.
    Incoming,
}

/// One pattern element: a vertex or an edge.
///
/// `filter` is written inside the element's own parentheses, which is what makes
/// a per-element predicate expressible at all — and, for `toolkit-db`, what
/// makes embedding scope per element possible.
#[derive(Clone, Debug)]
pub struct Element {
    variable: String,
    label: String,
    filter: Option<Condition>,
}

impl Element {
    /// An element bound to `variable`, matching `label`.
    pub fn new(variable: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            variable: variable.into(),
            label: label.into(),
            filter: None,
        }
    }

    /// Add a predicate inside this element's parentheses.
    ///
    /// Called more than once, the conditions are combined with `AND` — so a
    /// caller cannot
    /// replace a predicate that was already attached, only narrow it. That is
    /// what lets `toolkit-db` apply scope *after* a caller's own predicate and
    /// know it cannot be filtered back off.
    #[must_use]
    pub fn and_where(mut self, condition: Condition) -> Self {
        // An empty condition constrains nothing, and attaching it would render
        // as `WHERE TRUE` on every element — noise that also makes "this element
        // carries no predicate" indistinguishable from "this element carries a
        // vacuous one" when reading the SQL. Dropping it is safe precisely
        // because it adds nothing: it cannot be a caller removing scope.
        if condition.is_empty() {
            return self;
        }
        self.filter = Some(match self.filter.take() {
            Some(existing) => Condition::all().add(existing).add(condition),
            None => condition,
        });
        self
    }

    /// The variable this element is bound to.
    #[must_use]
    pub fn variable(&self) -> &str {
        &self.variable
    }

    /// The label this element matches.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The predicate attached so far, if any.
    #[must_use]
    pub fn filter(&self) -> Option<&Condition> {
        self.filter.as_ref()
    }
}

/// One hop: an edge and the vertex it reaches.
///
/// `pub` rather than `pub(crate)` only because this module is private, so the
/// two mean the same thing here and `clippy::redundant_pub_crate` prefers the
/// shorter one. Unlike the types around it, `Hop` is not re-exported.
#[derive(Clone, Debug)]
pub struct Hop {
    pub edge: Element,
    pub direction: Direction,
    pub target: Element,
}

/// A fixed-depth path pattern.
///
/// `PostgreSQL` 19's initial SQL/PGQ implementation supports fixed-depth
/// patterns only, so a pattern spells out its hops. Variable-depth traversal is
/// a different tool (a recursive CTE or a closure table), not a longer pattern.
#[derive(Clone, Debug)]
pub struct GraphPattern {
    pub(crate) head: Element,
    pub(crate) hops: Vec<Hop>,
}

impl GraphPattern {
    /// Start a pattern at `head`.
    #[must_use]
    pub fn new(head: Element) -> Self {
        Self {
            head,
            hops: Vec::new(),
        }
    }

    /// Follow `edge` in `direction` to `target`.
    #[must_use]
    pub fn hop(mut self, edge: Element, direction: Direction, target: Element) -> Self {
        self.hops.push(Hop {
            edge,
            direction,
            target,
        });
        self
    }

    /// Every element in the pattern, head first, in written order.
    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        std::iter::once(&self.head).chain(
            self.hops
                .iter()
                .flat_map(|hop| [&hop.edge, &hop.target].into_iter()),
        )
    }
}

/// One entry of the `COLUMNS` clause: a graph property projected as a column.
#[derive(Clone, Debug)]
pub struct ProjectedColumn {
    pub(crate) variable: String,
    pub(crate) property: String,
    pub(crate) alias: String,
}

impl ProjectedColumn {
    /// Project `variable.property` as `alias`.
    pub fn new(
        variable: impl Into<String>,
        property: impl Into<String>,
        alias: impl Into<String>,
    ) -> Self {
        Self {
            variable: variable.into(),
            property: property.into(),
            alias: alias.into(),
        }
    }

    /// The pattern variable the property is read off.
    #[must_use]
    pub fn variable(&self) -> &str {
        &self.variable
    }

    /// The projected property.
    #[must_use]
    pub fn property(&self) -> &str {
        &self.property
    }

    /// The output column name.
    #[must_use]
    pub fn alias(&self) -> &str {
        &self.alias
    }
}

/// A whole `GRAPH_TABLE` construct.
#[derive(Clone, Debug)]
pub struct GraphTable {
    pub(crate) graph: String,
    pub(crate) pattern: GraphPattern,
    pub(crate) columns: Vec<ProjectedColumn>,
}

impl GraphTable {
    /// A construct over the property graph `graph`.
    pub fn new(graph: impl Into<String>, pattern: GraphPattern) -> Self {
        Self {
            graph: graph.into(),
            pattern,
            columns: Vec::new(),
        }
    }

    /// Project a graph property into the result.
    #[must_use]
    pub fn column(mut self, column: ProjectedColumn) -> Self {
        self.columns.push(column);
        self
    }
}
