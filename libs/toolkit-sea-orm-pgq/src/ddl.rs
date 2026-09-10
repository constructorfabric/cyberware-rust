//! `CREATE` / `DROP PROPERTY GRAPH` DDL.
//!
//! A property graph is a schema object, created by a migration. This module
//! renders the statement; deciding *what* to declare is
//! `toolkit-db`'s job, driven by a single Rust declaration so that the labels a
//! `MATCH` uses and the tables behind them cannot drift apart
//! (`docs/arch/secure-orm/ADR/0002`, Policy 3).
//!
//! The one thing worth stating loudly: **a column absent from an element's
//! `PROPERTIES` list is invisible to `MATCH`.** Not an error — just silently
//! unfilterable, which for a scope column means the pattern cannot be scoped at
//! all. That failure mode is why generation exists.
//!
//! Fields are `pub(crate)` behind validating constructors, for the same reason
//! `ast` documents: these types are re-exported from the crate root, so `pub`
//! fields would let a caller assemble a declaration around the invariants the
//! constructors check — an endpoint whose key and referenced columns disagree
//! in arity, for instance, renders as DDL `PostgreSQL` rejects.

use sea_orm::sea_query::{Alias, IntoIden, PostgresQueryBuilder, QuotedBuilder};

use crate::error::PgqError;

fn write_ident(out: &mut String, name: &str) {
    let mut buffer = String::new();
    PostgresQueryBuilder.prepare_iden(&Alias::new(name).into_iden(), &mut buffer);
    out.push_str(&buffer);
}

fn write_ident_list(out: &mut String, names: &[String]) {
    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        write_ident(out, name);
    }
}

fn require_named(value: &str, what: &'static str) -> Result<(), PgqError> {
    if value.trim().is_empty() {
        return Err(PgqError::EmptyIdentifier { what });
    }
    Ok(())
}

/// The columns that identify an element row.
///
/// Composite on purpose in every graph this platform declares: a key carrying
/// the tenant means an edge structurally cannot join a vertex of another tenant,
/// before any scope predicate is applied.
#[derive(Clone, Debug)]
pub struct ElementKey(pub(crate) Vec<String>);

/// One vertex table and the label it is declared under.
#[derive(Clone, Debug)]
pub struct VertexTable {
    /// Table backing the element.
    pub(crate) table: String,
    /// Key columns.
    pub(crate) key: ElementKey,
    /// Label the pattern language addresses it by. One label per table: sharing
    /// a label across tables would give one label several security mappings.
    pub(crate) label: String,
    /// Columns exposed as properties. A scope column missing here cannot be
    /// filtered on inside a pattern.
    pub(crate) properties: Vec<String>,
}

impl VertexTable {
    /// An element over `table`, addressed as `label`, keyed on `key` and
    /// exposing `properties`.
    ///
    /// # Errors
    /// Returns [`PgqError::EmptyElementKey`] for a key with no columns — such an
    /// element could never be referenced by an endpoint — and
    /// [`PgqError::EmptyProperties`] for an empty property list, which would
    /// make the element unfilterable.
    pub fn new(
        table: impl Into<String>,
        key: Vec<String>,
        label: impl Into<String>,
        properties: Vec<String>,
    ) -> Result<Self, PgqError> {
        if key.is_empty() {
            return Err(PgqError::EmptyElementKey);
        }
        if properties.is_empty() {
            return Err(PgqError::EmptyProperties);
        }
        Ok(Self {
            table: table.into(),
            key: ElementKey(key),
            label: label.into(),
            properties,
        })
    }

    /// The label the pattern language addresses this element by.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The table backing the element.
    #[must_use]
    pub fn table(&self) -> &str {
        &self.table
    }

    /// The columns exposed as properties.
    #[must_use]
    pub fn properties(&self) -> &[String] {
        &self.properties
    }
}

/// Which vertex an edge endpoint points at.
#[derive(Clone, Debug)]
pub struct EndpointRef {
    /// Columns on the edge table.
    pub(crate) key: ElementKey,
    /// Vertex table referenced.
    pub(crate) table: String,
    /// Columns on that vertex table.
    pub(crate) references: ElementKey,
}

impl EndpointRef {
    /// An endpoint mapping `key` on the edge table to `references` on `table`.
    ///
    /// # Errors
    /// Returns [`PgqError::EmptyEndpointKey`] when either column list is empty,
    /// and [`PgqError::MismatchedEndpointArity`] when the two lists differ in
    /// length — both would render DDL `PostgreSQL` rejects.
    pub fn new(
        key: Vec<String>,
        table: impl Into<String>,
        references: Vec<String>,
    ) -> Result<Self, PgqError> {
        if key.is_empty() || references.is_empty() {
            return Err(PgqError::EmptyEndpointKey);
        }
        if key.len() != references.len() {
            return Err(PgqError::MismatchedEndpointArity {
                key: key.len(),
                references: references.len(),
            });
        }
        Ok(Self {
            key: ElementKey(key),
            table: table.into(),
            references: ElementKey(references),
        })
    }
}

/// One edge table, its label and its two endpoints.
#[derive(Clone, Debug)]
pub struct EdgeTable {
    /// The element itself.
    pub(crate) element: VertexTable,
    /// Source endpoint.
    pub(crate) source: EndpointRef,
    /// Destination endpoint.
    pub(crate) destination: EndpointRef,
}

impl EdgeTable {
    /// An edge over `element`, pointing from `source` to `destination`.
    #[must_use]
    pub fn new(element: VertexTable, source: EndpointRef, destination: EndpointRef) -> Self {
        Self {
            element,
            source,
            destination,
        }
    }

    /// The element common to vertices and edges: table, key, label, properties.
    #[must_use]
    pub fn element(&self) -> &VertexTable {
        &self.element
    }
}

/// A whole property-graph declaration.
#[derive(Clone, Debug)]
pub struct PropertyGraph {
    /// Name of the graph object.
    pub(crate) name: String,
    /// Vertex tables.
    pub(crate) vertices: Vec<VertexTable>,
    /// Edge tables.
    pub(crate) edges: Vec<EdgeTable>,
}

impl PropertyGraph {
    /// A graph named `name` over the given element tables.
    #[must_use]
    pub fn new(name: impl Into<String>, vertices: Vec<VertexTable>, edges: Vec<EdgeTable>) -> Self {
        Self {
            name: name.into(),
            vertices,
            edges,
        }
    }

    /// Render `CREATE PROPERTY GRAPH`.
    ///
    /// # Errors
    /// Returns [`PgqError`] when an identifier is empty or the graph declares no
    /// vertex tables — a graph with only edges cannot resolve an endpoint.
    pub fn create_statement(&self) -> Result<String, PgqError> {
        require_named(&self.name, "a property graph name")?;
        if self.vertices.is_empty() {
            return Err(PgqError::NoVertexTables);
        }

        let mut sql = String::from("CREATE PROPERTY GRAPH ");
        write_ident(&mut sql, &self.name);

        sql.push_str("\n  VERTEX TABLES (\n");
        for (index, vertex) in self.vertices.iter().enumerate() {
            if index > 0 {
                sql.push_str(",\n");
            }
            write_element(&mut sql, vertex)?;
        }
        sql.push_str("\n  )");

        if !self.edges.is_empty() {
            sql.push_str("\n  EDGE TABLES (\n");
            for (index, edge) in self.edges.iter().enumerate() {
                if index > 0 {
                    sql.push_str(",\n");
                }
                write_element_head(&mut sql, &edge.element)?;
                sql.push_str("\n      SOURCE KEY (");
                write_endpoint(&mut sql, &edge.source)?;
                sql.push_str("\n      DESTINATION KEY (");
                write_endpoint(&mut sql, &edge.destination)?;
                write_element_label(&mut sql, &edge.element);
            }
            sql.push_str("\n  )");
        }

        Ok(sql)
    }

    /// Render `DROP PROPERTY GRAPH IF EXISTS`.
    ///
    /// # Errors
    /// Returns [`PgqError::EmptyIdentifier`] for an unnamed graph.
    pub fn drop_statement(&self) -> Result<String, PgqError> {
        require_named(&self.name, "a property graph name")?;
        let mut sql = String::from("DROP PROPERTY GRAPH IF EXISTS ");
        write_ident(&mut sql, &self.name);
        Ok(sql)
    }
}

/// `table KEY (columns)` — the part that opens any element.
///
/// The list-emptiness invariants are enforced by the constructors; what remains
/// to check here are the identifiers themselves, which a constructor accepts as
/// arbitrary strings.
fn write_element_head(sql: &mut String, element: &VertexTable) -> Result<(), PgqError> {
    require_named(&element.table, "an element table name")?;
    require_named(&element.label, "an element label")?;

    sql.push_str("    ");
    write_ident(sql, &element.table);
    sql.push_str(" KEY (");
    write_ident_list(sql, &element.key.0);
    sql.push(')');
    Ok(())
}

/// `LABEL label PROPERTIES (columns)` — the part that closes any element.
///
/// Written separately because an edge's endpoint clauses sit **between** the two
/// halves: the grammar is `table KEY (…) SOURCE … DESTINATION … LABEL …`, and
/// emitting `LABEL` before `SOURCE` is a syntax error rather than a reordering
/// the parser tolerates.
fn write_element_label(sql: &mut String, element: &VertexTable) {
    sql.push_str("\n      LABEL ");
    write_ident(sql, &element.label);
    sql.push_str(" PROPERTIES (");
    write_ident_list(sql, &element.properties);
    sql.push(')');
}

/// A vertex element, whose two halves are adjacent.
fn write_element(sql: &mut String, element: &VertexTable) -> Result<(), PgqError> {
    write_element_head(sql, element)?;
    write_element_label(sql, element);
    Ok(())
}

fn write_endpoint(sql: &mut String, endpoint: &EndpointRef) -> Result<(), PgqError> {
    require_named(&endpoint.table, "an endpoint table name")?;
    write_ident_list(sql, &endpoint.key.0);
    sql.push_str(") REFERENCES ");
    write_ident(sql, &endpoint.table);
    sql.push_str(" (");
    write_ident_list(sql, &endpoint.references.0);
    sql.push(')');
    Ok(())
}
