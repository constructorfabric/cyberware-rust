//! Tests for the syntax layer.
//!
//! Every assertion is on rendered SQL. The two that matter most are structural
//! rather than substring-based: a correctly escaped identifier still *contains*
//! the dangerous substring, so searching for it proves nothing either way.

use sea_orm::sea_query::{Alias, Expr, ExprTrait as _, PostgresQueryBuilder, Query};
use sea_orm::{Condition, Value};

use crate::{
    Direction, EdgeTable, Element, EndpointRef, GraphPattern, GraphTable, PgqError,
    ProjectedColumn, PropertyGraph, VertexTable,
};

fn columns(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

fn simple() -> GraphTable {
    GraphTable::new(
        "kb",
        GraphPattern::new(Element::new("a", "node")).hop(
            Element::new("e", "edge"),
            Direction::Outgoing,
            Element::new("b", "node"),
        ),
    )
    .column(ProjectedColumn::new("b", "id", "neighbour"))
}

fn render(table: &GraphTable) -> String {
    table.render_for_test().expect("renders").0
}

#[test]
fn a_hop_renders_with_an_explicit_arrow() {
    let sql = render(&simple());
    assert_eq!(
        sql,
        r#""kb" MATCH ("a" IS "node")-["e" IS "edge"]->("b" IS "node") COLUMNS ("b"."id" AS "neighbour")"#
    );
}

/// An incoming hop is a distinct pattern, not a flag on the same one — and it
/// must not degrade into the undirected shorthand, which is orders of magnitude
/// slower for the same rows.
#[test]
fn an_incoming_hop_reverses_the_arrow() {
    let table = GraphTable::new(
        "kb",
        GraphPattern::new(Element::new("a", "node")).hop(
            Element::new("e", "edge"),
            Direction::Incoming,
            Element::new("b", "node"),
        ),
    )
    .column(ProjectedColumn::new("b", "id", "n"));
    let sql = render(&table);
    assert!(
        sql.contains(r#"<-["e" IS "edge"]-("b" IS "node")"#),
        "{sql}"
    );
    // The undirected form is `-[e]-`; neither arrowless shape may appear.
    assert!(!sql.contains(r#"]-("b"#) || sql.contains(r#"]-("b" IS "node")"#));
}

/// A predicate lives inside the element's own parentheses. This is what makes
/// per-element scoping expressible at all, so its placement is load-bearing
/// rather than cosmetic.
#[test]
fn a_predicate_goes_inside_its_own_element() {
    let table = GraphTable::new(
        "kb",
        GraphPattern::new(
            Element::new("a", "node")
                .and_where(Condition::all().add(Expr::val(7).eq(Expr::val(7)))),
        ),
    )
    .column(ProjectedColumn::new("a", "id", "n"));
    let sql = render(&table);
    assert!(
        sql.contains(r#"("a" IS "node" WHERE "#),
        "the predicate must be inside the element: {sql}"
    );
}

/// Attaching a second predicate narrows; it cannot replace the first. That is
/// what lets a security layer apply its predicate after a caller's and know the
/// caller cannot filter it back off.
#[test]
fn a_second_predicate_narrows_rather_than_replaces() {
    let element = Element::new("a", "node")
        .and_where(Condition::all().add(Expr::val(1).eq(Expr::val(1))))
        .and_where(Condition::all().add(Expr::val(2).eq(Expr::val(2))));
    let table = GraphTable::new("kb", GraphPattern::new(element))
        .column(ProjectedColumn::new("a", "id", "n"));
    let sql = render(&table);
    assert_eq!(sql.matches(" AND ").count(), 1, "both must survive: {sql}");
}

/// Values are bound, never written into the SQL. A pattern that interpolated
/// them would be an injection surface and would also defeat statement caching.
#[test]
fn values_are_bound_not_interpolated() {
    let table = GraphTable::new(
        "kb",
        GraphPattern::new(
            Element::new("a", "node")
                .and_where(Condition::all().add(Expr::col(Alias::new("id")).eq("o'brien"))),
        ),
    )
    .column(ProjectedColumn::new("a", "id", "n"));
    let (sql, values) = table.render_for_test().expect("renders");
    assert!(
        !sql.contains("o'brien"),
        "the value must not appear in the SQL: {sql}"
    );
    assert!(sql.contains("$1"), "expected a placeholder: {sql}");
    assert_eq!(values, vec![Value::from("o'brien")]);
}

/// Placeholders are numbered continuously across elements. Each condition
/// rendered on its own would restart at `$1`, and splicing those fragments would
/// bind the wrong value to the wrong element.
#[test]
fn placeholders_are_numbered_across_the_whole_pattern() {
    let table = GraphTable::new(
        "kb",
        GraphPattern::new(
            Element::new("a", "node")
                .and_where(Condition::all().add(Expr::col(Alias::new("id")).eq(1))),
        )
        .hop(
            Element::new("e", "edge")
                .and_where(Condition::all().add(Expr::col(Alias::new("kind")).eq(2))),
            Direction::Outgoing,
            Element::new("b", "node")
                .and_where(Condition::all().add(Expr::col(Alias::new("id")).eq(3))),
        ),
    )
    .column(ProjectedColumn::new("b", "id", "n"));
    let (sql, values) = table.render_for_test().expect("renders");
    assert!(
        sql.contains("$1") && sql.contains("$2") && sql.contains("$3"),
        "{sql}"
    );
    assert_eq!(values.len(), 3);
}

/// The regression the fragment-based renderer exists to prevent: an edge
/// element's predicate sits inside `[` `]`, which `sea_query`'s tokenizer
/// treats as a quoted string. Rendered to text and round-tripped through
/// `cust_with_values`, the edge's placeholder kept its inner numbering while
/// the outer statement renumbered everything else, and the edge's value was
/// dropped from the bound list — so the edge compared against a neighbouring
/// element's value.
///
/// Asserted on the *final statement*, the execution path, with elements that
/// bind different numbers of values: under a symmetric pattern the numbering
/// mistake is invisible because every element binds the same list.
#[test]
fn edge_values_survive_into_the_final_statement() {
    let table = GraphTable::new(
        "kb",
        GraphPattern::new(
            Element::new("a", "node").and_where(
                Condition::all()
                    .add(Expr::col(Alias::new("x")).eq(1))
                    .add(Expr::col(Alias::new("y")).eq(2)),
            ),
        )
        .hop(
            Element::new("e", "edge")
                .and_where(Condition::all().add(Expr::col(Alias::new("z")).eq(3))),
            Direction::Outgoing,
            Element::new("b", "node").and_where(
                Condition::all()
                    .add(Expr::col(Alias::new("x")).eq(4))
                    .add(Expr::col(Alias::new("y")).eq(5)),
            ),
        ),
    )
    .column(ProjectedColumn::new("b", "id", "n"));

    let statement = Query::select()
        .column(Alias::new("n"))
        .from(table.into_table_ref("g").expect("renders"))
        .to_owned();
    let (sql, values) = statement.build(PostgresQueryBuilder);

    // The edge's own placeholder, numbered continuously with its neighbours.
    assert!(sql.contains(r#""z" = $3"#), "{sql}");
    assert!(!sql.contains("$6"), "only five values exist: {sql}");
    assert_eq!(
        values.0,
        vec![
            Value::from(1),
            Value::from(2),
            Value::from(3),
            Value::from(4),
            Value::from(5),
        ],
        "every element's values must be bound, in pattern order"
    );
}

/// Everything outside a quoted identifier, with the identifiers removed.
///
/// Substring searches over the whole statement prove nothing about escaping: a
/// correctly escaped identifier still *contains* the dangerous text, which is
/// exactly what makes a naive assertion pass on both the safe and the unsafe
/// rendering. So the check is structural — strip the quoted regions, honouring
/// `""` as an escaped quote, and assert on what is left, which is the only part
/// `PostgreSQL` reads as syntax.
fn outside_identifiers(sql: &str) -> Result<String, &'static str> {
    let mut out = String::new();
    let mut chars = sql.chars().peekable();
    let mut inside = false;
    while let Some(c) = chars.next() {
        match (c, inside) {
            ('"', false) => inside = true,
            ('"', true) => {
                if chars.peek() == Some(&'"') {
                    // A doubled quote is one literal character of the name, so
                    // the identifier does not end here.
                    chars.next();
                } else {
                    inside = false;
                }
            }
            (other, false) => out.push(other),
            (_, true) => {}
        }
    }
    if inside {
        // An unclosed identifier is itself a break-out: the rest of the
        // statement has been swallowed into a name.
        return Err("an identifier was never closed");
    }
    Ok(out)
}

/// A hostile name must render as exactly one identifier — it must not be able
/// to close its own quoting and become syntax — in **every** position the
/// construct quotes: the graph name, a vertex label, an edge label, an element
/// variable, a projected property and a projected alias. Each position is
/// exercised alone, so a position that bypassed `write_ident` fails on its own
/// assertion instead of hiding behind the others.
#[test]
fn a_hostile_identifier_renders_as_one_escaped_identifier_in_every_position() {
    let hostile = r#"a" IS "node") COLUMNS ("x"."y" AS "z"); DROP TABLE t; --"#;
    let one_vertex = |graph: &str, variable: &str, label: &str| {
        GraphTable::new(graph, GraphPattern::new(Element::new(variable, label)))
    };
    let positions: Vec<(&str, GraphTable)> = vec![
        (
            "graph name",
            one_vertex(hostile, "a", "node").column(ProjectedColumn::new("a", "id", "n")),
        ),
        (
            "vertex label",
            one_vertex("kb", "a", hostile).column(ProjectedColumn::new("a", "id", "n")),
        ),
        (
            "edge label",
            GraphTable::new(
                "kb",
                GraphPattern::new(Element::new("a", "node")).hop(
                    Element::new("e", hostile),
                    Direction::Outgoing,
                    Element::new("b", "node"),
                ),
            )
            .column(ProjectedColumn::new("b", "id", "n")),
        ),
        (
            "element variable",
            one_vertex("kb", hostile, "node").column(ProjectedColumn::new(hostile, "id", "n")),
        ),
        (
            "projected property",
            one_vertex("kb", "a", "node").column(ProjectedColumn::new("a", hostile, "n")),
        ),
        (
            "projected alias",
            one_vertex("kb", "a", "node").column(ProjectedColumn::new("a", "id", hostile)),
        ),
    ];

    for (position, table) in positions {
        let sql = render(&table);
        let syntax = outside_identifiers(&sql).expect("identifiers must be balanced");
        assert_eq!(
            syntax.matches(" MATCH ").count(),
            1,
            "{position}: the payload became a second MATCH: {syntax}"
        );
        assert_eq!(
            syntax.matches(" COLUMNS (").count(),
            1,
            "{position}: the payload became a second COLUMNS: {syntax}"
        );
        assert!(
            !syntax.contains("DROP"),
            "{position}: the payload escaped its identifier: {syntax}"
        );
        assert!(
            !syntax.contains("--"),
            "{position}: the payload introduced a comment: {syntax}"
        );
    }
}

/// The DDL has an identifier path of its own, and the ADR names it too: the
/// graph name, table names, labels, key columns, properties and endpoint
/// references must each render as one escaped identifier in
/// `CREATE PROPERTY GRAPH`, and the graph name in `DROP PROPERTY GRAPH`.
#[test]
fn a_hostile_identifier_in_the_ddl_renders_as_one_escaped_identifier() {
    let hostile = r#"kb"); DROP TABLE t; --"#;
    let element = |table: &str, key: &str, label: &str, property: &str| {
        VertexTable::new(table, columns(&[key]), label, columns(&[property]))
            .expect("a valid element")
    };
    let graph = PropertyGraph::new(
        hostile,
        vec![element(hostile, hostile, hostile, hostile)],
        vec![EdgeTable::new(
            element("graph_edge", "id", "edge", "id"),
            EndpointRef::new(columns(&[hostile]), hostile, columns(&[hostile]))
                .expect("a valid source"),
            EndpointRef::new(columns(&["dst"]), hostile, columns(&[hostile]))
                .expect("a valid destination"),
        )],
    );

    let create = graph.create_statement().expect("renders");
    let syntax = outside_identifiers(&create).expect("identifiers must be balanced");
    assert_eq!(
        syntax.matches("CREATE PROPERTY GRAPH").count(),
        1,
        "{syntax}"
    );
    assert_eq!(syntax.matches("VERTEX TABLES (").count(), 1, "{syntax}");
    assert_eq!(syntax.matches("EDGE TABLES (").count(), 1, "{syntax}");
    assert!(
        !syntax.contains("DROP"),
        "the payload escaped its identifier: {syntax}"
    );
    assert!(
        !syntax.contains("--"),
        "the payload introduced a comment: {syntax}"
    );

    let drop = graph.drop_statement().expect("renders");
    let syntax = outside_identifiers(&drop).expect("identifiers must be balanced");
    assert_eq!(
        syntax, "DROP PROPERTY GRAPH IF EXISTS ",
        "only the keyword frame may survive outside the identifier"
    );
}

/// The scanner the test above relies on has to be right, or that test is
/// decorative. Two renderings a `format!`-built renderer would have produced
/// must both be visible to it.
#[test]
fn the_escaping_check_can_actually_fail() {
    // Break-out that leaves the quoting balanced: the payload closes the
    // element and opens a whole second COLUMNS clause. Written as the
    // counterfactual a `format!`-built renderer would have produced, so the
    // example cannot drift from what it is meant to represent.
    let payload = r#"a" IS "node") COLUMNS ("x"."y" AS "z"#;
    let balanced = format!(r#""kb" MATCH ("{payload}" IS "node") COLUMNS ("a"."id" AS "n")"#);
    let syntax = outside_identifiers(&balanced).expect("balanced");
    assert!(
        syntax.matches(" COLUMNS (").count() > 1,
        "the scanner must see the second clause: {syntax}"
    );

    // And the same for the payload the positive test actually uses, whose odd
    // quote count leaves everything after it swallowed into a name. The scanner
    // reports that rather than quietly returning a truncated view.
    let with_comment = r#"a" IS "node") COLUMNS ("x"."y" AS "z"); DROP TABLE t; --"#;
    let unbalanced =
        format!(r#""kb" MATCH ("{with_comment}" IS "node") COLUMNS ("a"."id" AS "n")"#);
    assert!(outside_identifiers(&unbalanced).is_err());
}

/// The construct name is a keyword, so it must NOT be quoted — quoting it makes
/// the statement a syntax error rather than a slow query, which is the kind of
/// mistake that is easy to make once identifiers are being escaped everywhere.
#[test]
fn the_construct_name_is_not_quoted() {
    let table_ref = simple().into_table_ref("g").expect("renders");
    let sql = Query::select()
        .expr(Expr::val(1))
        .from(table_ref)
        .to_string(PostgresQueryBuilder);
    assert!(sql.contains("GRAPH_TABLE("), "{sql}");
    assert!(!sql.contains(r#""GRAPH_TABLE""#), "{sql}");
}

/// The construct is usable where a table is, which is the whole point of it
/// being a table function.
#[test]
fn it_can_be_placed_in_a_from_clause() {
    let table_ref = simple().into_table_ref("g").expect("renders");
    let sql = Query::select()
        .expr(Expr::val(1))
        .from(table_ref)
        .to_string(PostgresQueryBuilder);
    assert!(sql.contains(r#"AS "g""#), "{sql}");
}

#[test]
fn a_construct_without_columns_is_refused() {
    let table = GraphTable::new("kb", GraphPattern::new(Element::new("a", "node")));
    assert_eq!(table.render_for_test().unwrap_err(), PgqError::NoColumns);
}

#[test]
fn an_empty_identifier_is_refused() {
    let table = GraphTable::new("", GraphPattern::new(Element::new("a", "node")))
        .column(ProjectedColumn::new("a", "id", "n"));
    assert!(matches!(
        table.render_for_test().unwrap_err(),
        PgqError::EmptyIdentifier { .. }
    ));
}

/// One variable naming two elements would make a predicate on it ambiguous —
/// and would make "scope is attached per element" false, since two elements
/// would share a qualifier.
#[test]
fn a_repeated_variable_is_refused() {
    let table = GraphTable::new(
        "kb",
        GraphPattern::new(Element::new("a", "node")).hop(
            Element::new("e", "edge"),
            Direction::Outgoing,
            Element::new("a", "node"),
        ),
    )
    .column(ProjectedColumn::new("a", "id", "n"));
    assert_eq!(
        table.render_for_test().unwrap_err(),
        PgqError::DuplicateVariable("a".to_owned())
    );
}

// ─────────────────────────────── DDL ───────────────────────────────

fn graph_ddl() -> PropertyGraph {
    let node = VertexTable::new(
        "graph_node",
        columns(&["tenant_id", "id"]),
        "node",
        columns(&["tenant_id", "id"]),
    )
    .expect("a valid vertex");
    PropertyGraph::new(
        "kb",
        vec![node],
        vec![EdgeTable::new(
            VertexTable::new(
                "graph_edge",
                columns(&["tenant_id", "id"]),
                "edge",
                columns(&["tenant_id", "id"]),
            )
            .expect("a valid edge element"),
            EndpointRef::new(
                columns(&["tenant_id", "src"]),
                "graph_node",
                columns(&["tenant_id", "id"]),
            )
            .expect("a valid source"),
            EndpointRef::new(
                columns(&["tenant_id", "dst"]),
                "graph_node",
                columns(&["tenant_id", "id"]),
            )
            .expect("a valid destination"),
        )],
    )
}

/// An edge's clauses must come in grammar order: `KEY`, then `SOURCE`, then
/// `DESTINATION`, then `LABEL`. Asserted as an ordering rather than by
/// containment, because a statement with every clause present in the wrong order
/// still contains all of them — the first version of this test passed while the
/// generated DDL was a syntax error `PostgreSQL` rejected at `LABEL`.
#[test]
fn an_edge_declares_its_clauses_in_grammar_order() {
    let sql = graph_ddl().create_statement().expect("renders");
    let edges = sql.split("EDGE TABLES").nth(1).expect("has edge tables");
    let key = edges.find(" KEY (").expect("KEY");
    let source = edges.find("SOURCE KEY").expect("SOURCE");
    let destination = edges.find("DESTINATION KEY").expect("DESTINATION");
    let label = edges.find("LABEL").expect("LABEL");
    assert!(
        key < source && source < destination && destination < label,
        "clauses out of grammar order in: {edges}"
    );
}

/// The endpoint keys carry the tenant column, which is what makes an edge
/// structurally unable to join a vertex of another tenant — before any scope
/// predicate is applied.
#[test]
fn the_ddl_declares_composite_endpoint_keys() {
    let sql = graph_ddl().create_statement().expect("renders");
    assert!(
        sql.contains(
            r#"SOURCE KEY ("tenant_id", "src") REFERENCES "graph_node" ("tenant_id", "id")"#
        ),
        "{sql}"
    );
    assert!(sql.contains("CREATE PROPERTY GRAPH \"kb\""), "{sql}");
    assert!(
        sql.contains(r#"LABEL "node" PROPERTIES ("tenant_id", "id")"#),
        "{sql}"
    );
}

/// A graph with no exposed properties would be unfilterable, so it is refused
/// rather than declared — the failure it prevents is silent.
#[test]
fn an_element_without_properties_is_refused() {
    assert_eq!(
        VertexTable::new("graph_node", columns(&["id"]), "node", Vec::new()).unwrap_err(),
        PgqError::EmptyProperties
    );
}

/// An endpoint with mismatched arity renders DDL `PostgreSQL` rejects, so the
/// constructor refuses it — in a different crate the caller could not be relied
/// on to check.
#[test]
fn a_mismatched_endpoint_is_refused_at_construction() {
    assert_eq!(
        EndpointRef::new(
            columns(&["tenant_id", "src"]),
            "graph_node",
            columns(&["id"])
        )
        .unwrap_err(),
        PgqError::MismatchedEndpointArity {
            key: 2,
            references: 1
        }
    );
    assert_eq!(
        EndpointRef::new(Vec::new(), "graph_node", columns(&["id"])).unwrap_err(),
        PgqError::EmptyEndpointKey
    );
}

/// A graph with no vertex tables cannot resolve any endpoint.
#[test]
fn a_graph_without_vertices_is_refused() {
    let graph = PropertyGraph::new("kb", Vec::new(), Vec::new());
    assert_eq!(
        graph.create_statement().unwrap_err(),
        PgqError::NoVertexTables
    );
}

#[test]
fn the_drop_statement_is_idempotent() {
    assert_eq!(
        graph_ddl().drop_statement().expect("renders"),
        r#"DROP PROPERTY GRAPH IF EXISTS "kb""#
    );
}
