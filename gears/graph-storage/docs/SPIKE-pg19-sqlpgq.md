# Spike Report — SQL/PGQ on PostgreSQL 19 beta2 with pgvector


<!-- toc -->

- [Environment](#environment)
- [Findings](#findings)
  - [F1. pgvector on PG19 beta2: works](#f1-pgvector-on-pg19-beta2-works)
  - [F2. Variable-length quantifiers: not supported (as expected)](#f2-variable-length-quantifiers-not-supported-as-expected)
  - [F3. Undirected edge patterns plan catastrophically — use directed unions](#f3-undirected-edge-patterns-plan-catastrophically--use-directed-unions)
  - [F4. Multi-hop chain patterns have path semantics — unusable for neighborhoods](#f4-multi-hop-chain-patterns-have-path-semantics--unusable-for-neighborhoods)
  - [F5. The viable PGQ shape: directed 1-hop primitive + per-hop dedup](#f5-the-viable-pgq-shape-directed-1-hop-primitive--per-hop-dedup)
  - [F6. Single-statement composition: confirmed](#f6-single-statement-composition-confirmed)
  - [F7. DDL notes](#f7-ddl-notes)
- [Consequences for the gear](#consequences-for-the-gear)
- [Caveats](#caveats)
- [Follow-up: measured again on the gear's own stand (2026-08-18)](#follow-up-measured-again-on-the-gears-own-stand-2026-08-18)
- [Follow-up: SQL/PGQ implemented in the gear (2026-08-18)](#follow-up-sqlpgq-implemented-in-the-gear-2026-08-18)
  - [SQL/PGQ needs no `sea_query` fork](#sqlpgq-needs-no-sea_query-fork)
  - [What a property graph is, read off the catalogs](#what-a-property-graph-is-read-off-the-catalogs)
  - [F3 re-measured on the gear's schema: 2350x](#f3-re-measured-on-the-gears-schema-2350x)
  - [A pattern cannot compute its own seeds](#a-pattern-cannot-compute-its-own-seeds)
  - [The tenant predicate is not symmetric](#the-tenant-predicate-is-not-symmetric)
  - [End to end, SQL/PGQ is not the slow option](#end-to-end-sqlpgq-is-not-the-slow-option)
  - [The composition claim, executed](#the-composition-claim-executed)

<!-- /toc -->

**Date:** 2026-08-13 · **Status:** complete · **Companion to:** [ADR-0001](./ADR/0001-cpt-cf-graph-storage-adr-single-postgres-store.md) (Confirmation gate)

**Question:** does the SQL/PGQ target backend hold up — does pgvector build and run on PG19, does `GRAPH_TABLE` serve the gear's fixed-depth neighborhood shapes within the latency budget, and does graph+vector+FTS compose in one statement?

**Verdict: the PG19 stack is usable today — the gear can start on SQL/PGQ from v1.** pgvector builds and works on PG19 beta2. SQL/PGQ is viable, but only in the *hop-primitive* shape (directed 1-hop `GRAPH_TABLE` chained with per-hop dedup) — naive multi-hop chain patterns and the undirected shorthand are unusable on hub-heavy graphs in the initial PG19 implementation. The recursive-CTE backend measured ~2.5x faster on pure hop expansion; both leave large headroom under the interactive budget at kernel level, so ADR-0001 puts SQL/PGQ in front for fixed-depth shapes from v1 (composition and declarativity win) with the CTE backend carrying variable depth and serving as fallback. Single-statement KNN → graph → FTS composition is confirmed. These are traversal-kernel timings on a synthetic graph; the traversal-latency NFR is claimed only after an end-to-end benchmark of the complete neighborhood endpoint at the reference profile (see F5).

## Environment

| Item | Value |
|---|---|
| PostgreSQL | 19beta2 (Debian `19~beta2-1.pgdg13+1`, official `postgres:19beta2` image) |
| pgvector | 0.8.6, built from source, master commit `5219575` (PG19 support upstream: issue #1005 closed 2026-07-29) |
| Host | WSL2 dev machine, Docker; `shared_buffers=1GB`, `work_mem=64MB` |
| Dataset | 200,000 nodes / 659,991 edges / 50,000 x 384-dim normalized embeddings; 12 node types, 8 edge types; hub-skewed destinations (power-law-like) — prototype-shaped `kb` schema without AGE columns |
| Load | Seed + indexes in well under a minute; HNSW build over 50k vectors: 3.2 s |

## Findings

### F1. pgvector on PG19 beta2: works

`CREATE EXTENSION vector` succeeds (0.8.6); HNSW cosine index builds and serves KNN. The PG19 gate for the target backend is only the GA timeline, not extension compatibility.

### F2. Variable-length quantifiers: not supported (as expected)

`MATCH (a)-[IS edge]->{1,3}(b)` fails with `element pattern quantifier is not supported`. Bounded variable-depth stays on the CTE backend until PG20-class SQL/PGQ, exactly as ADR-0001 assumes.

### F3. Undirected edge patterns plan catastrophically — use directed unions

`(a IS node)-[IS edge]-(b IS node)` is planned as "enumerate all 200k candidate vertices, probe edge existence per pair" (400k index searches, ~123 ms per single-seed 1-hop, warm cache). The equivalent `UNION ALL` of two directed matches plans as clean index nested loops: **0.2 ms**. The PGQ backend must always emit direction-explicit patterns.

### F4. Multi-hop chain patterns have path semantics — unusable for neighborhoods

`(a)-[]-(x)-[]-(y)-[]-(b)` enumerates all *paths*, not reachable nodes: exact-3-hop from a 12-degree seed took **24.4 s**; from a hub seed it exceeded 2 minutes (intermediate hops pass through hubs regardless of the seed). Fixed-depth neighborhood queries must never be written as single chain patterns on hub-heavy graphs.

### F5. The viable PGQ shape: directed 1-hop primitive + per-hop dedup

Chaining `GRAPH_TABLE` 1-hop expansions through CTE stages with `DISTINCT` + visited-set exclusion (lateral join over the previous frontier) matches CTE results exactly and performs well.

Depth<=3 undirected neighborhood, random seeds (hubs included), single client, 25 s pgbench runs, per-transaction latency log:

| Backend | n | p50 | p95 | p99 | max |
|---|---|---|---|---|---|
| Recursive CTE (visited-set BFS) | 14,384 | 0.75 ms | 4.06 ms | 30.5 ms | 48.9 ms |
| PGQ hop-primitive chain | 6,094 | 2.16 ms | 8.75 ms | 59.2 ms | 81.0 ms |

These are **kernel timings, not an NFR result**: they measure hop expansion on a synthetic 200k-node / 660k-edge graph, whereas `cpt-cf-graph-storage-nfr-traversal-latency` covers the complete neighborhood response (bounded expansion, degree calculation and ordering, phantom filtering, retained-edge hydration, optional metric annotations, serialization) at the 100k-node / 500k-edge reference profile with a 1,000-node budget. Read this table as evidence that the directed one-hop PGQ primitive is viable next to the CTE primitive, with both leaving large headroom under the budget. The NFR is claimed only after an end-to-end benchmark of the full neighborhood endpoint at the required shape, depth, budget, representative dense seeds, and p95 procedure. The PGQ overhead here is per-hop query-shape bookkeeping the planner cannot yet collapse; it does not decide the default — ADR-0001 does (SQL/PGQ for fixed-depth shapes from v1, CTE for variable depth and fallback).

### F6. Single-statement composition: confirmed

One SQL statement combining pgvector HNSW KNN (top-5 seeds) -> PGQ 1-hop expansion -> node-type filter: **20.7 ms**. Adding an FTS predicate (`websearch_to_tsquery`) on the seed selection: **39.6 ms**. This is the capability AGE could not offer across the agtype boundary and the core reason SQL/PGQ is the target backend.

### F7. DDL notes

`CREATE PROPERTY GRAPH` works over the prototype schema unchanged; the `SOURCE/DESTINATION ... REFERENCES` clause takes the vertex *element* name (`node`), not the schema-qualified table name.

## Consequences for the gear

1. The gear starts directly on PostgreSQL 19 with the SQL/PGQ backend active from v1 — this spike is what de-risked that choice ahead of PG19 GA (the gear ships earlier than GA; deployments pin the beta image and a pgvector source revision until then). Recorded in ADR-0001.
2. The `GraphQueryPort` PGQ backend must generate direction-explicit, hop-primitive SQL (F3, F5) — recorded in DESIGN § Traversal Backend Sketch.
3. The `studio-graph-storage` prototype has been migrated to this exact stack (PG19 beta2 + pgvector from source, AGE removed, sql/pgq hop backends), giving the gear a running reference implementation.
4. Re-run this spike at PG19 GA (planner may improve; percentiles will shift) and at PG20 betas (quantifier support would collapse the hop chain into one pattern).

## Caveats

Warm cache, single client, synthetic graph (uniform + power-law mix), no tenant predicates, WSL2 laptop — numbers are for shape comparison, not capacity planning. The reproduction assets (Dockerfile, seed, benchmark scripts) are in the spike workspace and are trivially recreatable from this report.

## Follow-up: measured again on the gear's own stand (2026-08-18)

This report deliberately stopped at kernel timings. A Rust development stand for
the gear has since re-measured on its real schema — composite `(tenant_id, id)`
keys, 200,003 nodes / 600,000 edges, hub-skewed destinations — and extended the
measurement end to end:

| Shape | p50 | p95 |
|---|---|---|
| Two scoped queries (shipped implementation) | 0.183 ms | 0.371 ms |
| Single statement with a scoped CTE | 0.213 ms | 0.432 ms |
| SQL/PGQ `GRAPH_TABLE`, direction-explicit union | 0.402 ms | 0.645 ms |

Depth-3 neighbourhood through HTTP, debug build, naive visited set: p95 89 ms
against the 1 s NFR. That covers bounded expansion, budget enforcement and
authorization — still not degree ordering, hydration or metric annotations, so
the NFR claim remains partial, but the part this spike could not measure is now
measured.

Two findings that change how the backends are written:

* Composite element keys are accepted by `CREATE PROPERTY GRAPH`, and because the
  edge's source and destination keys carry `tenant_id`, no pattern can cross a
  tenant boundary even before a scope predicate is applied.
* The loopback deployment makes the extra round trip of the two-query shape
  nearly free, which is why it leads the table here; on a network with a
  millisecond round trip the single-statement shapes would lead instead. The
  ranking is a property of the deployment, not of the shapes.

## Follow-up: SQL/PGQ implemented in the gear (2026-08-18)

The stand has since grown a working SQL/PGQ traversal backend, selectable
alongside the other two, plus single-statement hybrid retrieval. Six findings
change or sharpen what this report concluded.

### SQL/PGQ needs no `sea_query` fork

Reaching `GRAPH_TABLE` from Rust looked like it required an AST node `sea_query`
does not have. It does not. `TableRef::FunctionCall` puts a call in the `FROM`
clause, `Func::Custom` renders the name **unquoted** — load-bearing, since
`GRAPH_TABLE(...)` parses and `"GRAPH_TABLE"(...)` is a syntax error — and
`Expr::cust_with_values` renders arbitrary text while binding its values.

The remaining obstacle is therefore policy, not tooling: `Expr::cust` is raw
SQL, which gear code may not write. The construct's production home is inside
`toolkit-db`, which the platform CTE policy already exempts for dialect-specific
assembly.

### What a property graph is, read off the catalogs

A property graph is a relation with no storage (`relkind = 'g'`, `relnatts = 0`)
whose five new catalogs record which columns join to which. Ours holds
`graph_node` as a vertex with key `{1,2}` and `graph_edge` as an edge with
`{1,5} → {1,2}` and `{1,6} → {1,2}` — attribute numbers, so `(tenant_id, id)`
and `(tenant_id, src_node_id)`.

`GRAPH_TABLE` is expanded at parse analysis into that join. The one-hop pattern
plans as `graph_node ⋈ graph_edge ⋈ graph_node` carrying
`tenant_id = graph_edge.tenant_id` — a predicate nobody wrote, derived from the
composite keys. So the abstraction costs nothing at runtime, there is no second
store to keep in sync, and every hop is a join. The last point is why chain
patterns explode (F4) and why variable length cannot be expanded (F2).

### F3 re-measured on the gear's schema: 2350x

The undirected shorthand costs more than this report's kernel numbers implied.
Same seed, same ten rows, on the gear's tables: `(a)-[e]-(b)` plans as a
parallel sequential scan of `graph_edge` at **734.9 ms**, the two directed
patterns unioned at **0.312 ms**. The gear's pattern builder therefore has no
undirected variant at all — offering one would be offering a trap.

### A pattern cannot compute its own seeds

`PostgreSQL` 19 rejects subqueries inside `GRAPH_TABLE` outright:

```text
ERROR:  subqueries within GRAPH_TABLE reference are not supported
```

`IN (SELECT ...)`, `= ANY(ARRAY(SELECT ...))` and `CROSS JOIN LATERAL
GRAPH_TABLE (...)` are all refused, the last with a bare syntax error. What
works is a comma join with a correlated reference — an implicit lateral:

```sql
FROM (SELECT id FROM graph_node ORDER BY embedding <=> $q LIMIT $k) AS knn_seeds,
     GRAPH_TABLE (kb_pgq MATCH (a IS node)-[e IS edge]->(b IS node)
       WHERE a.id = knn_seeds.id AND ... COLUMNS (b.id AS neighbour)) AS g
```

This single syntactic fact is what makes single-statement hybrid retrieval
possible on this release at all.

### The tenant predicate is not symmetric

For a one-hop pattern seeded by id: the source predicate alone fences the walk,
the target predicate alone fences it too, and **neither** present leaks — a
caller then reads whichever tenant owns the ids named. Composite element keys
tie both ends of an edge to one tenant, so anchoring either end anchors the
other. The gear emits both anyway, as defence against a schema that stops
carrying `tenant_id` in the element keys, and guards that redundancy with a text
assertion because no execution test can distinguish one predicate from two.

### End to end, SQL/PGQ is not the slow option

The per-hop table above compares a single statement against a single query,
which understates the two-query shape's cost: it pays a round trip per hop and
ships an intermediate frontier across the process boundary. Measured end to end
over HTTP on the same fixture, 40 fixed seeds, with the three backends returning
byte-identical results across 120 requests:

| depth | two scoped queries | scoped CTE | `GRAPH_TABLE` |
|---|---|---|---|
| 1 | p50 3.8 / p95 4.0 ms | p50 3.1 / p95 6.0 ms | p50 3.7 / p95 4.1 ms |
| 2 | p50 6.7 / p95 7.9 ms | p50 5.3 / p95 7.5 ms | p50 6.6 / p95 7.9 ms |
| 3 | p50 13.7 / p95 52.8 ms | p50 9.0 / p95 33.1 ms | p50 11.5 / p95 35.3 ms |

SQL/PGQ lands second of three, close behind the CTE hop and ahead of the shipped
two-query one. Re-taking these is `dev/bench-hops.sh`.

### The composition claim, executed

Hybrid retrieval — nearest neighbours by cosine distance, one hop out of each
seed in both directions, the reached nodes filtered by full text and ranked by
distance — runs as one statement whose plan is index-driven at every stage,
including the HNSW probe, at 8.1 ms. Against the same answer assembled from
three round trips, 25 runs with ids verified identical each time: p50 11.0 ms
against 14.1 ms, p95 15.1 ms against 18.9 ms.

Twenty percent on loopback is the smaller half of the argument. The larger half
is that the intermediate frontier never crosses the process boundary and the
planner sees the whole shape at once. This is the capability ADR-0001 chose
SQL/PGQ for, and it is now demonstrated rather than assumed.
