# Graph Engine Alternatives — Decision Report

**Date:** 2026-08-12 · **Status:** superseded — historical evidence, not the current decision

> **Superseded by [ADR-0001](./ADR/0001-cpt-cf-graph-storage-adr-single-postgres-store.md)
> and the [PG19 spike](./SPIKE-pg19-sqlpgq.md) (2026-08-13).** The verdict below recommends
> Apache AGE and defers PostgreSQL 19; the spike run the next day found SQL/PGQ in core
> viable, and ADR-0001 removed AGE. This document is retained unedited as the evidence that
> was available on 2026-08-12 — it records what was known at decision time and is referenced
> from ADR-0001 for that reason. Read its verdict, exit strategy and recommended sequence as
> history; the current backend contract is ADR-0001's.
**Scope:** alternatives to the current PostgreSQL + Apache AGE + pgvector stack for `studio-graph-storage`.

> **Verdict: stay on PG + AGE + pgvector.** Under the hard OSI-license requirement it is the
> only production-supportable single-engine stack covering graph + vector + relational search +
> multi-tenancy. Its ceiling is a *workload shape* (deep / variable-length traversals), not a
> data size — and our workload is fixed-depth. Re-evaluate ArcadeDB in Q1 2027; treat FalkorDB
> as license-blocked; do not rebuild the Qdrant + Nebula + PG trio.

---

## 1. Requirements

Unchanged since project start. **Open source license is the primary gate** (this is what
disqualified Neo4j Enterprise despite it being the best technical fit in a previous project).

| # | Requirement | Notes |
|---|---|---|
| 1 | **OSI-approved open source** | BSL, SSPL, Elastic License do NOT qualify |
| 2 | 1M–500M nodes | aggregate across tenants |
| 3 | Vector search | 384-dim (MiniLM-L6-v2), tenant-filtered KNN |
| 4 | Regular search | full-text + property lookups |
| 5 | Cypher-like query language | Gremlin/AQL/DQL are a mismatch |
| 6 | Multi-tenancy | isolated graph per system |
| 7 | Local dev + Kubernetes deploy | single container locally; HA + backups in k8s |

## 2. Scoreboard (12 engines, verified August 2026)

Licenses were verified against repository license files, not marketing pages — several changed
during 2024–2026.

| Engine | OSI license | 1M–500M | Vector | Cypher | Multi-tenant | K8s | Health 2026 | Deciding fact |
|---|---|---|---|---|---|---|---|---|
| **PG + AGE + pgvector** | ✅ Apache 2.0 | ⚠ shape-limited | ✅ | ⚠ subset | ✅ | ✅ CNPG | ⚠ slow cadence | Only fully-OSI single engine with all four capabilities |
| **ArcadeDB** | ✅ Apache 2.0 | ⚠ unproven | ⚠ beta | ✅ 97.8% TCK v9 | ✅ | ✅ Helm+Raft | ✅ active | Passes all criteria on paper; every pillar < 8 months old |
| FalkorDB | ❌ SSPL | ⚠ RAM-bound | ✅ | ✅ subset | ✅ 10K+ graphs | ✅ | ✅ active | Best measured performance; license-blocked |
| Neo4j Community | ✅ GPLv3 | ✅ disk, 34B cap | ✅ | ✅ reference | ❌ single DB | ⚠ no HA | ✅ | Multi-DB / RBAC / clustering / online backup = Enterprise-only |
| Dgraph v25 | ✅ Apache 2.0 | ✅ distributed | ✅ | ❌ DQL/GraphQL | ✅ namespaces | ✅ Helm+HA | ⚠ | Fully Apache 2.0 since v25 — but no Cypher |
| LadybugDB (Kùzu fork) | ✅ MIT | ✅ LDBC SF100 | ✅ | ✅ | ⚠ app-managed | ⚠ embedded | ❌ 10-mo fork | Kùzu archived Oct 2025 (Apple acqui-hire) |
| Memgraph | ❌ BSL (tightened Jan 2026) | ⚠ RAM = 2× data | ✅ | ✅ | ❌ Enterprise | ⚠ HA = Enterprise | ✅ | Jan 2026 license change removed the SaaS grant |
| Apache HugeGraph | ✅ Apache 2.0 | ✅ | ❌ in dev | ⚠ Gremlin-first | ✅ + RBAC | ⚠ no Helm | ⚠ | Vector index not shipped in server core |
| TuGraph | ✅ Apache 2.0 | ✅ | ⚠ immature | ✅ + ISO GQL | ✅ | ❌ Docker only | ❌ | No release since 03/2025, mid-rewrite |
| NebulaGraph | ⚠ open-core | ✅ | ❌ Enterprise v5 | ⚠ nGQL | ✅ | ✅ operator | ⚠ | Vector + ISO GQL are Enterprise-only |
| ArangoDB | ❌ BSL 1.1 | ✅ | ✅ flagged | ❌ AQL | ✅ | ✅ operator | ⚠ pivoting | Community binary: no commercial production, 100 GiB cap |
| JanusGraph | ✅ Apache 2.0 | ✅ | ❌ | ❌ Gremlin | ⚠ | ❌ 3 clusters | ⚠ | Needs Cassandra + Elasticsearch — heaviest ops burden |

Also eliminated: **OrientDB** (effectively unmaintained; founder points to ArcadeDB),
**Kùzu** (archived), **GraphScope** (analytics platform, not a storage service),
**PuppyGraph / Ultipa** (proprietary), **pgGraph / OneSparse / pgRouting** (no Cypher —
complements to AGE, not replacements). Within PostgreSQL, AGE remains the only Cypher
implementation.

## 3. Finalists: deep-dive findings

Both finalists were researched at source level (issue trackers, license texts, funding records)
and smoke-tested in Docker with an identical seeded dataset: 200k nodes, 600k edges, 50k of the
nodes carrying 384-dim embeddings. Test scripts are reusable for a full PoC.

### 3.1 Smoke test, head to head

| Metric | FalkorDB | ArcadeDB |
|---|---|---|
| Load throughput (800k entities) | ~53k entities/s | ~24k entities/s |
| Indexed point lookup p50 | 0.24 ms | 2.16 ms (incl. HTTP) |
| 2-hop aggregation p50 / p95 | 203 / 259 ms | 484 / 514 ms |
| Var-length `[*1..3]` p50 (warm) | 0.5 ms | 2.0 ms |
| Vector index build (50k × 384) | 29.1 s | ❌ **not creatable via SQL/HTTP** (Java API only) |
| KNN k=10 p50 | 0.5 ms | n/a |
| Hybrid KNN→graph, one query | ✅ 0.8 ms | untestable |
| Memory after load | 421 MiB RSS (graph itself: 114 MB) | 2.18 GiB RSS (JVM cap) |
| Failed Cypher probes | undirected `shortestPath`, `EXISTS { pattern }` | none (broadest coverage) |
| Tenant isolation | ✅ graph-per-key | ✅ database-per-tenant (stronger) |

### 3.2 FalkorDB — fast, but license-blocked and RAM-bound

**Strengths (measured):** fastest engine tested; one-query hybrid (vector KNN seeds flow into
`MATCH`); in-engine GraphBLAS algorithms (PageRank, betweenness, WCC, shortest paths) that would
replace the NetworkX export; best-in-field multi-tenancy (graph-per-key, per-tenant
dump/restore, 10K+ graphs per instance).

**Blockers:**

1. **SSPL, read aggressively by its own vendor.** MongoDB (the license author) exempts SaaS
   apps that merely *use* the database; FalkorDB's FAQ claims copyleft applies "if you use
   FalkorDB as part of a service you make available to others (e.g., in the cloud or as an
   API)". Our service *is* a graph API inside a commercial product — the closer end users get
   to graph-query functionality, the stronger the argument that Section 13 triggers, which
   would require open-sourcing the entire service stack. SSPL is untested in court; the
   licensor's interpretation frames any dispute; commercial licensing is contact-sales only.
   **With a paid product on the roadmap this is a first-order blocker, not a footnote.**
2. **Fully in-RAM.** Vendor math: 1M nodes + 50M edges ≈ 3.3 GB; 384-dim embeddings add
   ~1.8 GB per 1M vectors. 500M nodes ≈ 1 TB+ — before the one documented production case at
   scale ([FalkorDB#978](https://github.com/FalkorDB/FalkorDB/issues/978)): 13 GB of data
   needing a 48 GiB pod (fragmentation ratio 15). No spill-to-disk; a single graph cannot be
   sharded (Redis Cluster distributes whole graphs).
3. **ANN is post-filter only** — KNN runs first with fixed k, filters after; selective tenant
   filters can starve results. pgvector 0.8 iterative scans handle this case better.
4. **Redis-grade durability:** ≤1 s crash-loss window at sane fsync; async replication (loss
   on failover); per-query atomicity only. Weaker than Postgres WAL on every axis.
5. **Vendor: ~7 people, $3M seed (2024), no Series A** — while maintaining a C engine and a
   Rust rewrite in parallel. No independent benchmark exists above ~1M nodes.

### 3.3 ArcadeDB — best fit on paper, beta in practice

**Strengths:** Apache 2.0 with a public no-relicense pledge, nothing enterprise-gated;
broadest Cypher of any candidate (97.8% openCypher TCK v9, confirmed broadest in our probes);
database-per-tenant with per-DB users and quotas; broadest in-engine algorithm library
(Louvain/Leiden, node2vec, k-shortest-paths); native `vector.fuse()` with RRF — in principle
exactly our hybrid-search pipeline, in-engine.

**Blockers (as of August 2026):**

1. **Every load-bearing subsystem is under 8 months old:** native Cypher (Jan 2026 — before
   that, a translation through an abandoned Gremlin layer), Raft HA on Apache Ratis (Apr 2026),
   JVector vector index (May 2026). Recent fixed bugs include a leader losing committed writes
   ([#5492](https://github.com/ArcadeData/arcadedb/issues/5492)), Cypher writes bypassing Raft
   ([#5655](https://github.com/ArcadeData/arcadedb/issues/5655)), and HNSW silently degrading
   to linear scans at ~200K vectors ([#5391](https://github.com/ArcadeData/arcadedb/issues/5391)).
   An open bug crash-loops a whole Raft cluster on bulk insert
   ([#5933](https://github.com/ArcadeData/arcadedb/issues/5933)) — exactly our load pattern.
2. **Vector index is not creatable in server mode** (SQL/HTTP) — confirmed hands-on; Java
   embedded API only. Incremental HNSW insertion is an open feature request (ingestion is
   rebuild-based).
3. **"Postgres wire protocol" ≠ compatibility:** simple query mode only, no SSL, and the
   dialect is ArcadeDB SQL — none of our psycopg3 + AGE code carries over. psycopg3 connects,
   psycopg2 does not.
4. **Bus factor ≈ 1:** ~80% of human commits are one person (Luca Garulli); company is
   founder-owned, unfunded. Monthly releases are a counterweight, but OrientDB's post-founder
   history is the cautionary tale.
5. No independent evidence above a few million nodes; scale claims are vendor benchmarks.

**Action:** re-evaluate in Q1 2027 — check server-mode vector DDL, incremental HNSW, and
whether #5933-class HA bugs stopped appearing. Smoke-test scripts are ready for a 10M-node pilot.

## 4. Current stack: where it actually breaks on the way to 500M

The known AGE ceiling — variable-length paths bypass indexes (`[*..n]` → sequential scans;
production mitigation documented by
[Trendyol](https://medium.com/trendyol-tech/migrating-graph-operations-to-apache-age-from-writes-to-reads-3b8334628e1c)) —
**mostly does not apply to us**: our queries are fixed-depth neighbourhoods, and property
filtering/search happens on the relational `kb` side with normal indexes, not inside agtype.
The dual-write design (relational `kb` = source of truth, AGE = disposable traversal mirror)
already sidesteps AGE's most fragile parts, including bulk writes (direct SQL, ~1000× faster
than Cypher `CREATE`).

Growth map by aggregate size:

| Stage | What breaks | Mitigation |
|---|---|---|
| ≤ 10M | Nothing structural | Tune `maintenance_work_mem`, autovacuum on re-ingest |
| 10–50M | **NetworkX metrics** (export won't fit/finish) — our earliest real wall, and no alternative engine fixes it | Degree and simple metrics as SQL aggregates over `kb.edge`; centrality incrementally/sampled on a worker |
| 10–50M | HNSW builds take hours | `CREATE INDEX CONCURRENTLY`, build windows |
| 50–100M | Vector index RAM: 384-dim ≈ 500–700 B/vector → 100M ≈ 50–70 GB that wants to be cached | Bigger page cache, or pgvectorscale (StreamingDiskANN, disk-friendly), or quantization; count *actual* vectors (nodes + chunks, both nullable) |
| 50–100M | Backup/PITR size (kb + AGE mirror ≈ 2× storage); 3–4-hop fan-out latency | Ops planning; staged queries; denormalized adjacency for hot paths |
| 100–500M | Single-writer ingest throughput: each `kb.node` insert updates 5+ indexes (PK, unique, GIN×2, tsvector, HNSW) | Partition `kb.node`/`kb.edge` by system; drop-index→load→rebuild for bulk; graph-per-system already shards the mirror naturally |

Uncharted territory: nobody has published results running thousands of AGE graphs in one
database. For many small tenants prefer `tenant_id` + RLS over graph-per-tenant.

**Decision triggers** (measure these, not node counts): p95 of 2–3-hop API queries, ingest
throughput, HNSW build duration, metrics job duration.

## 5. Exit strategy: SQL/PGQ in PostgreSQL 19+

PG19 (GA ~Sept 2026) ships SQL:2023 property graph queries in core: `CREATE PROPERTY GRAPH`
defines a **view-like graph over existing relational tables** — no agtype, no mirror, normal
indexes, `EXPLAIN`, and RLS. Directly over our schema:

```sql
CREATE PROPERTY GRAPH kb_pgq
  VERTEX TABLES (
    kb.node KEY (id) LABEL node PROPERTIES (node_key, name, gts_type_id)
  )
  EDGE TABLES (
    kb.edge KEY (id)
      SOURCE KEY (src_node_id) REFERENCES kb.node (id)
      DESTINATION KEY (dst_node_id) REFERENCES kb.node (id)
      LABEL edge PROPERTIES (gts_type_id)
  );

SELECT g.name, g.nk
FROM GRAPH_TABLE (kb_pgq
  MATCH (a IS node)-[IS edge]->(IS node)-[IS edge]->(b IS node)
  WHERE a.node_key = 'doc-123' AND b.gts_type_id = 7
  COLUMNS (b.name AS name, b.node_key AS nk)
) g;
```

`GRAPH_TABLE` is an ordinary table expression: it joins with `embedding <=> $1` and
`search @@ query` in one SQL statement — graph + vector + FTS without crossing the agtype
boundary.

- **PG19 limitation:** fixed-length patterns only (no `[*1..3]`, no shortest path — expected
  PG20+). Covers our fixed-depth API; arbitrary depth stays on AGE/recursive CTEs for now.
- **pgvector on PG19:** orthogonal — compile the extension against PG19, `CREATE EXTENSION
  vector`. The laggard is AGE (new-PG-major support historically arrives months late; PG16 is
  still on AGE 1.6.0). Realistic path: stay on PG16–18 while the mirror is needed; the PG19+
  move happens either when AGE supports it or when PGQ covers enough that **the mirror is
  dropped entirely** — that is the AGE exit, with zero data migration because `kb` is already
  the source of truth.

## 6. Rejected: rebuilding Qdrant + NebulaGraph + PG at 10M+

Considered and rejected (it was the previous project's architecture; its pain was the reason
for this stack). 10M is not where PG+AGE breaks, and the trio's costs start on day one:

1. **Every write is a distributed saga** across three stores with no shared transactions —
   partial failures leave orphaned vectors / dangling nodes; requires outbox/CDC, idempotent
   retries, and permanent reconciliation jobs.
2. **The ID-mapping table is self-inflicted:** one shared UUID as Nebula VID *and* Qdrant point
   ID removes PG's "connector" role entirely. If a two-engine design is ever built, build it
   that way — as rebuildable projections off `kb`, synced by CDC, with only PG backed up.
3. **Hybrid search fans out to all three systems** (Qdrant KNN → Nebula expand → PG hydration
   + FTS, fused in app code); latencies compose, filters don't push down.
4. **~3× the k8s footprint:** Nebula HA (metad×3 + storaged×3 + graphd×2) + Qdrant cluster ×3
   + CNPG ≈ 12–15 pods, three operators, three upgrade cycles — and **no consistent
   cross-store backup exists**; restore = restore PG, replay the rest.
5. **Three incompatible tenancy models** (Nebula spaces are heavyweight per tenant; Qdrant
   recommends payload-partitioned single collections; PG uses RLS/schemas).
6. **Nebula's gaps are unchanged:** nGQL (partial openCypher), vector search and ISO GQL are
   Enterprise-only — the very reason Qdrant was needed remains unfixed in the open core.

Justified only if *both* specialized needs materialize simultaneously (~1B+ edges with deep
traversal load **and** 100M+ vectors at high QPS) — two orders of magnitude past today. The
intermediate step is always: swap **one** mirror (AGE → specialized graph engine), keep vectors
in pgvector until it measurably fails.

## 7. Recommended sequence

1. **Now:** stay on PG + AGE + pgvector. Coding standards: no unbounded `*` in production
   Cypher; filters in `MATCH` patterns (not `WHERE`) when querying AGE; bulk writes via SQL
   (already the case).
2. **Before ~10M:** move graph metrics off NetworkX (SQL aggregates + incremental jobs).
3. **Sept 2026:** evaluate PG19 SQL/PGQ against the fixed-depth part of the API.
4. **Q1 2027:** re-evaluate ArcadeDB (server-mode vector DDL, incremental HNSW, HA stability).
5. **~50M vectors:** add pgvectorscale; pgvector 0.8 iterative scans for tenant-filtered KNN.
6. **If hot multi-hop traversals become the measured bottleneck:** swap the *mirror*, not the
   system of record. FalkorDB is the strongest mirror candidate at per-tenant ≤50M nodes —
   gated on a legal opinion on SSPL (or a commercial license) and a fragmentation/BGSAVE load
   test.
7. **If vectors outgrow Postgres:** PG stays source of truth; shared UUID as the key
   everywhere; CDC (Debezium) to the external index. No mapping tables.

## 8. Key sources

- Trendyol: [Neo4j → Apache AGE migration in production](https://medium.com/trendyol-tech/migrating-graph-operations-to-apache-age-from-writes-to-reads-3b8334628e1c)
- Apache AGE: [releases](https://github.com/apache/age/releases) · [2026 roadmap](https://github.com/apache/age/discussions/2305) · index issues [#562](https://github.com/apache/age/issues/562), [#1000](https://github.com/apache/age/issues/1000), [#1235](https://github.com/apache/age/issues/1235)
- PostgreSQL 19 SQL/PGQ: [depesz overview](https://www.depesz.com/2026/07/31/waiting-for-postgresql-19-sql-property-graph-queries-sql-pgq/)
- pgvector at scale: [pgvectorscale vs Pinecone, 50M vectors](https://www.tigerdata.com/blog/pgvector-is-now-as-fast-as-pinecone-at-75-less-cost) · [0.8 iterative index scans](https://docs.pgedge.com/pgvector/v0-8-0/iterative-index-scans/)
- FalkorDB: [license FAQ](https://docs.falkordb.com/References/license.html) · [MongoDB's narrower SSPL reading](https://www.mongodb.com/legal/licensing/server-side-public-license/faq) · [Cypher limitations](https://docs.falkordb.com/cypher/known-limitations.html) · [fragmentation #978](https://github.com/FalkorDB/FalkorDB/issues/978) · [independent benchmark](https://aimultiple.com/graph-databases)
- ArcadeDB: [repo](https://github.com/ArcadeData/arcadedb) · [native Cypher](https://arcadedb.com/blog/native-opencypher/) · [vector search](https://docs.arcadedb.com/arcadedb/concepts/vector-search) · issues [#5391](https://github.com/ArcadeData/arcadedb/issues/5391), [#5492](https://github.com/ArcadeData/arcadedb/issues/5492), [#5933](https://github.com/ArcadeData/arcadedb/issues/5933)
- Memgraph: [BSL text, amended Jan 2026](https://github.com/memgraph/memgraph/blob/master/licenses/BSL.txt) · Neo4j: [edition comparison](https://neo4j.com/docs/operations-manual/current/introduction/) · Dgraph: [v25 all-Apache announcement](https://hypermode.com/blog/dgraph-v25-preview) · Kùzu: [archival coverage](https://www.theregister.com/software/2025/10/14/kuzudb-graph-database-abandoned-community-mulls-options/1142229) · NebulaGraph: [vector = Enterprise v5.1+](https://medium.com/@nebulagraph/nebulagraph-enterprise-v5-1-embeds-vector-search-for-ai-grade-data-fusion-3ba698e0c410)
- Full interactive report with the smoke-test details: internal artifact (see PR/issue links).

---
*Compiled from seven research sweeps (four landscape, two finalist deep dives, one hands-on
Docker smoke test) on 2026-08-11/12. License states change frequently — re-verify before acting
on this after 2026.*
