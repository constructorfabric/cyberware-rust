#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! SQL/PGQ (`GRAPH_TABLE`) syntax for `sea_query`.
//!
//! `sea_query` models no part of SQL/PGQ — verified against the pinned
//! `sea_query` 1.0.2, where the only occurrences of "graph" are in the `CYCLE`
//! clause documentation. This crate fills that gap: a typed AST for
//! `GRAPH_TABLE … MATCH … COLUMNS (…)` and for the `CREATE`/`DROP PROPERTY
//! GRAPH` DDL, plus a renderer that produces something a `sea_query`
//! statement can put in its `FROM`.
//!
//! # What this crate deliberately does not know
//!
//! Anything about security. There is no `AccessScope`, no `ScopableEntity` and
//! no database runner here, and there never should be: `toolkit-db` decides
//! *which* predicate is mandatory on an element, this crate only knows *how* to
//! write one. That split is what makes the syntax testable without a security
//! context, and it is the layering
//! `docs/arch/secure-orm/ADR/0002` fixes.
//!
//! Consequently: **using this crate directly from a gear is unsafe by
//! construction.** Nothing here embeds scope, so a pattern built straight from
//! this AST traverses whatever the graph contains. Gears go through
//! `toolkit-db`'s secure graph builder, which is the only thing that guarantees
//! every element carries a scope predicate.
//!
//! # Two properties the renderer is responsible for
//!
//! **Identifiers are escaped, never formatted.** Graph names, labels, element
//! variables and property names all become identifiers in the emitted SQL, and
//! all of them go through `sea_query`'s own quoting
//! ([`QuotedBuilder::prepare_iden`]) rather than through `format!`. A hostile
//! name therefore renders as exactly one quoted identifier.
//!
//! **Values are bound, never interpolated.** Element predicates arrive as
//! `sea_query` [`Condition`]s and are handed to the enclosing statement's
//! builder as expressions, so placeholders are numbered continuously across
//! the whole statement and the values travel as parameters. The `render`
//! module documents why they must not be pre-rendered to text instead.
//!
//! # Directions are explicit
//!
//! There is no undirected element and no undirected shorthand, because
//! `(a)-[e]-(b)` plans as a parallel sequential scan of the edge table:
//! measured at 735 ms for one such element and 7967 ms for two, against ~1.5 ms
//! for the same result set written with arrows. An undirected hop is two
//! directed patterns.
//!
//! [`QuotedBuilder::prepare_iden`]: sea_orm::sea_query::QuotedBuilder::prepare_iden
//! [`Condition`]: sea_orm::Condition

mod ast;
mod ddl;
mod error;
mod render;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;

pub use ast::{Direction, Element, GraphPattern, GraphTable, ProjectedColumn};
pub use ddl::{EdgeTable, EndpointRef, PropertyGraph, VertexTable};
pub use error::PgqError;
