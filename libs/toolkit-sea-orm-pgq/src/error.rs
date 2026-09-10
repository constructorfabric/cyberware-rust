//! The one error type the AST and the DDL builders share.
//!
//! Its own module so that `ast` and `ddl` stay independent of each other: both
//! need this type, neither needs the other.

/// Something the AST refuses to render.
///
/// `#[non_exhaustive]` because this is the public error enum of a published
/// crate: new refusals will appear as the syntax coverage grows, and adding a
/// variant must not be a breaking change for every downstream `match`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PgqError {
    /// A pattern with no `COLUMNS` produces no output columns, so the enclosing
    /// query has nothing to select.
    #[error("a GRAPH_TABLE needs at least one projected column")]
    NoColumns,
    /// An identifier that cannot name anything.
    #[error("{what} must not be empty")]
    EmptyIdentifier {
        /// Which identifier was empty.
        what: &'static str,
    },
    /// Two elements of one pattern share a variable, so a predicate on that
    /// variable would be ambiguous.
    #[error("element variable `{0}` is used twice in one pattern")]
    DuplicateVariable(String),
    /// A property graph that declares no vertex tables cannot resolve any edge
    /// endpoint, so there is nothing valid to create.
    #[error("a property graph needs at least one vertex table")]
    NoVertexTables,
    /// An element key with no columns cannot be referenced by an endpoint.
    #[error("an element key needs at least one column")]
    EmptyElementKey,
    /// An element whose `PROPERTIES` list is empty is unfilterable — a column
    /// absent from that list is invisible to `MATCH`, silently.
    #[error("an element needs at least one exposed property")]
    EmptyProperties,
    /// An endpoint that names no columns on either side references nothing.
    #[error("an edge endpoint needs both its own columns and the columns it references")]
    EmptyEndpointKey,
    /// An endpoint whose key and referenced columns differ in length would
    /// render DDL `PostgreSQL` rejects.
    #[error(
        "an edge endpoint must reference as many columns as it carries ({key} vs {references})"
    )]
    MismatchedEndpointArity {
        /// Columns the endpoint carries on the edge table.
        key: usize,
        /// Columns it references on the vertex table.
        references: usize,
    },
}
