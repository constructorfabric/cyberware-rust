# cf-gears-toolkit-sea-orm-pgq

SQL/PGQ (`GRAPH_TABLE`) syntax for `sea_query` — a typed AST for
`GRAPH_TABLE … MATCH … COLUMNS (…)` and for the `CREATE`/`DROP PROPERTY GRAPH`
DDL, plus a renderer that produces something a `sea_query` statement can put in
its `FROM`. Targets PostgreSQL 19+, the first release to implement SQL/PGQ.

This crate is deliberately security-agnostic: there is no `AccessScope`, no
entity trait and no database runner here. `toolkit-db` decides *which*
predicate is mandatory on an element; this crate only knows *how* to write one.

**Using this crate directly from a gear is unsafe by construction.** Nothing
here embeds scope, so a pattern built straight from this AST traverses whatever
the graph contains. Gears go through `toolkit-db`'s secure graph builder
(`toolkit_db::secure::pgq`), which is the only thing that guarantees every
element carries a scope predicate. The design is recorded in
`docs/arch/secure-orm/ADR/0002`.

## Guarantees the renderer is responsible for

- **Identifiers are escaped, never formatted** — every graph name, label,
  variable and property goes through `sea_query`'s own quoting.
- **Values are bound, never interpolated** — element predicates are handed to
  the enclosing statement's builder as expressions, so placeholders are
  numbered continuously across the whole statement.
- **Directions are explicit** — there is no undirected element form; the
  undirected shorthand plans as a sequential scan of the edge table and is
  orders of magnitude slower than two directed patterns.
