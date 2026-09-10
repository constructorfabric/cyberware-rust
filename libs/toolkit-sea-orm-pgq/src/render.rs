//! Rendering the AST into SQL.
//!
//! Two invariants live here, and both are pinned by tests:
//!
//! * every identifier goes through `sea_query`'s own quoting, never `format!`;
//! * every value travels as a bound parameter, never as text.
//!
//! # Why the construct is assembled from fragments
//!
//! The construct cannot be rendered to one string and round-tripped through
//! `Expr::cust_with_values`: that constructor re-tokenises its template, and
//! `sea_query`'s tokenizer treats `[` / `]` as a string-delimiter pair (MSSQL
//! bracket quoting). A whole `["e" IS "edge" WHERE … $n]` edge element comes
//! out as one quoted token and is re-emitted verbatim — its `$n` keeps the
//! pre-render numbering while the outer statement renumbers everything else,
//! and the value it referred to is silently dropped from the bound list.
//!
//! So the body is one [`Expr::cust_with_exprs`] whose template holds only
//! `$n` markers, never brackets. The raw syntax (brackets, arrows, quoted
//! identifiers) rides in [`Expr::cust`] fragments, which render verbatim
//! without tokenisation, and each element predicate is handed to the enclosing
//! builder as a plain [`Condition`]-derived expression — the builder then does
//! the placeholder numbering itself, continuously across the whole statement.
//!
//! [`Expr::cust_with_exprs`]: sea_orm::sea_query::Expr::cust_with_exprs
//! [`Expr::cust`]: sea_orm::sea_query::Expr::cust
//! [`Condition`]: sea_orm::Condition

// Writing into an in-memory `String` cannot fail, so the `Result` a `write!`
// into one returns is an `Ok` nobody can act on — and `unwrap` on it would be
// noise claiming a failure mode that does not exist.
#![allow(clippy::let_underscore_must_use)]

use std::collections::BTreeSet;
use std::fmt::Write as _;

#[cfg(test)]
use sea_orm::Value;
use sea_orm::sea_query::{
    Alias, Expr, Func, IntoIden, PostgresQueryBuilder, QuotedBuilder, TableRef,
};
#[cfg(test)]
use sea_orm::sea_query::{QueryBuilder, SqlWriterValues};

use crate::ast::{Direction, Element, GraphTable, ProjectedColumn};
use crate::error::PgqError;

/// The construct's name. Rendered raw rather than as an identifier, because it
/// is a keyword: quoting it would make the statement a syntax error.
const GRAPH_TABLE: &str = "GRAPH_TABLE";

/// Write `name` as one quoted, escaped identifier.
///
/// Goes through `QuotedBuilder::prepare_iden`, which is the same path every
/// identifier in a `sea_query` statement takes. `Alias::new` on a runtime string
/// always takes the escaping branch, so a name containing the quote character
/// comes out with it doubled rather than closing the identifier early.
fn write_ident(out: &mut String, name: &str) {
    let mut buffer = String::new();
    PostgresQueryBuilder.prepare_iden(&Alias::new(name).into_iden(), &mut buffer);
    out.push_str(&buffer);
}

/// Refuse an identifier that cannot name anything.
fn require_named(value: &str, what: &'static str) -> Result<(), PgqError> {
    if value.trim().is_empty() {
        return Err(PgqError::EmptyIdentifier { what });
    }
    Ok(())
}

/// The construct body under assembly: raw syntax fragments interleaved with
/// element predicates, exactly as `cust_with_exprs` will consume them.
#[derive(Default)]
struct Fragments {
    parts: Vec<Expr>,
    /// Raw text accumulated since the last predicate. Flushed into `parts` as a
    /// verbatim [`Expr::cust`] fragment whenever a predicate interrupts it.
    text: String,
}

impl Fragments {
    fn flush(&mut self) {
        if !self.text.is_empty() {
            self.parts.push(Expr::cust(std::mem::take(&mut self.text)));
        }
    }

    /// Append a predicate as an expression fragment the enclosing builder will
    /// render — and therefore number and bind — itself.
    fn condition(&mut self, condition: &sea_orm::Condition) {
        self.flush();
        self.parts.push(condition.clone().into());
    }

    /// One `$n`-marker-per-fragment template, with the markers separated by
    /// spaces so the tokenizer cannot glue a marker to its neighbour.
    fn into_expr(mut self) -> Expr {
        self.flush();
        let mut template = String::new();
        for index in 1..=self.parts.len() {
            if index > 1 {
                template.push(' ');
            }
            // Writing into a `String` cannot fail.
            let _ = write!(template, "${index}");
        }
        Expr::cust_with_exprs(template, self.parts)
    }
}

/// Write one element: `(var IS label WHERE …)` or `[var IS label WHERE …]`.
fn write_element(
    fragments: &mut Fragments,
    element: &Element,
    brackets: (char, char),
) -> Result<(), PgqError> {
    require_named(element.variable(), "an element variable")?;
    require_named(element.label(), "an element label")?;

    fragments.text.push(brackets.0);
    write_ident(&mut fragments.text, element.variable());
    fragments.text.push_str(" IS ");
    write_ident(&mut fragments.text, element.label());
    if let Some(filter) = element.filter() {
        fragments.text.push_str(" WHERE");
        fragments.condition(filter);
    }
    fragments.text.push(brackets.1);
    Ok(())
}

/// Write the `COLUMNS` clause.
fn write_columns(fragments: &mut Fragments, columns: &[ProjectedColumn]) -> Result<(), PgqError> {
    if columns.is_empty() {
        return Err(PgqError::NoColumns);
    }
    fragments.text.push_str(" COLUMNS (");
    for (index, column) in columns.iter().enumerate() {
        require_named(&column.variable, "a projected variable")?;
        require_named(&column.property, "a projected property")?;
        require_named(&column.alias, "a projected column alias")?;
        if index > 0 {
            fragments.text.push_str(", ");
        }
        write_ident(&mut fragments.text, &column.variable);
        fragments.text.push('.');
        write_ident(&mut fragments.text, &column.property);
        fragments.text.push_str(" AS ");
        write_ident(&mut fragments.text, &column.alias);
    }
    fragments.text.push(')');
    Ok(())
}

/// Reject a pattern that binds one variable twice.
///
/// A predicate on such a variable would be ambiguous, and for `toolkit-db` it
/// would also make "scope is attached per element" untrue: two elements would
/// share one qualifier.
fn require_distinct_variables(table: &GraphTable) -> Result<(), PgqError> {
    let mut seen = BTreeSet::new();
    for element in table.pattern.elements() {
        if !seen.insert(element.variable()) {
            return Err(PgqError::DuplicateVariable(element.variable().to_owned()));
        }
    }
    Ok(())
}

/// Assemble the inside of `GRAPH_TABLE ( … )` as one expression.
fn body_expr(table: &GraphTable) -> Result<Expr, PgqError> {
    require_named(&table.graph, "a property graph name")?;
    require_distinct_variables(table)?;

    let mut fragments = Fragments::default();

    write_ident(&mut fragments.text, &table.graph);
    fragments.text.push_str(" MATCH ");
    write_element(&mut fragments, &table.pattern.head, ('(', ')'))?;

    for hop in &table.pattern.hops {
        // The arrow is always explicit. There is no undirected form to fall
        // back to, by design — see the crate documentation.
        let (before, after) = match hop.direction {
            Direction::Outgoing => ("-", "->"),
            Direction::Incoming => ("<-", "-"),
        };
        fragments.text.push_str(before);
        write_element(&mut fragments, &hop.edge, ('[', ']'))?;
        fragments.text.push_str(after);
        write_element(&mut fragments, &hop.target, ('(', ')'))?;
    }

    write_columns(&mut fragments, &table.columns)?;

    Ok(fragments.into_expr())
}

/// Render the inside of `GRAPH_TABLE ( … )` and the values it binds.
///
/// Renders the same expression tree the execution path hands to the enclosing
/// statement, so what a test asserts on cannot drift from what the server sees.
/// Test-only: the execution path goes through [`table_ref`], which never
/// renders the body on its own.
#[cfg(test)]
pub fn body(table: &GraphTable) -> Result<(String, Vec<Value>), PgqError> {
    let expr = body_expr(table)?;
    // `$` numbered: the same placeholder style `PostgresQueryBuilder` produces.
    let mut writer = SqlWriterValues::new("$", true);
    PostgresQueryBuilder.prepare_expr(&expr, &mut writer);
    let (sql, values) = writer.into_parts();
    Ok((sql, values.into_iter().collect()))
}

/// Render the construct as an aliased `FROM` item.
pub fn table_ref(table: &GraphTable, alias: &str) -> Result<TableRef, PgqError> {
    require_named(alias, "a GRAPH_TABLE alias")?;

    // `Func::cust` renders its name raw, which is what a keyword needs, and the
    // single argument becomes the parenthesised body. The element predicates
    // inside it are ordinary expressions of the enclosing statement, so their
    // placeholders are numbered by the statement's own builder and their values
    // land in the statement's own bound list.
    let call = Func::cust(Alias::new(GRAPH_TABLE)).arg(body_expr(table)?);
    Ok(TableRef::FunctionCall(call, Alias::new(alias).into_iden()))
}

impl GraphTable {
    /// Render as a `FROM` item aliased `alias`.
    ///
    /// # Errors
    /// Returns [`PgqError`] for a construct that cannot name anything: no
    /// projected columns, an empty identifier, or a variable used twice.
    pub fn into_table_ref(self, alias: &str) -> Result<TableRef, PgqError> {
        table_ref(&self, alias)
    }

    /// Render the construct's SQL and bound values, without executing it.
    ///
    /// Exposed for tests: assertions belong on rendered SQL, because a predicate
    /// that never reaches the database would satisfy a `Debug`-form assertion
    /// and still leak. Crate-private — a published crate must not ship a
    /// test-only entry point to every downstream consumer.
    ///
    /// # Errors
    /// As [`Self::into_table_ref`].
    #[cfg(test)]
    pub(crate) fn render_for_test(&self) -> Result<(String, Vec<Value>), PgqError> {
        body(self)
    }
}
