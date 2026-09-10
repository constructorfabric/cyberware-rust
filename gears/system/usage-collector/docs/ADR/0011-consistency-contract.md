---
status: accepted
date: 2026-05-31
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-07-20 by Virtuozzo International GmbH

# Consistency contract for usage-collector read/write paths

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Floor-and-ceiling split (eventual no-bound floor; plugin profile ceiling)](#floor-and-ceiling-split-eventual-no-bound-floor-plugin-profile-ceiling)
  - [Monotonic-reads-per-`(tenant, usage-type)` floor](#monotonic-reads-per-tenant-usage-type-floor)
  - [Bounded-staleness floor (e.g., ≤ N ms across all backends)](#bounded-staleness-floor-eg--n-ms-across-all-backends)
  - [Read-your-writes floor (session-affinity required of every plugin)](#read-your-writes-floor-session-affinity-required-of-every-plugin)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-consistency-contract`

## Context and Problem Statement

Usage Collector (UC) routes ingestion and query through distinct Plugin Service
Provider Interface (SPI) methods so each active plugin can place them on
isolated backend pools (read replicas, separate executor pools). That isolation is the
structural source of queryability lag between the ingestion ack path and the
subsequent Query SPI path, but nothing in DESIGN states the consequence as a
contract. PRD §9 acceptance criteria informally combine ingestion-ack latency
and queryability into one "ingestion-latency-bounded freshness" phrase, which
hides that the two are governed by different mechanisms with different bounds.

Two intra-plugin invariants ARE pinned down in DESIGN and `plugin-spi.md` and
remain in force: the `(tenant_id, gts_id, idempotency_key, created_at)` dedup
tuple is permanently visible to subsequent ingestion attempts after `Acknowledged` —
the dedup-key tuple is preserved permanently and independently of the plugin's
record-body retention policy —, and a `deactivate` of
an accepted row commits atomically with its depth-1 compensation cascade in a
single backend transaction. Neither invariant says
anything about whether the same record is visible to a subsequent raw or
aggregated query.

DESIGN §3.12.7 (Event architecture N/A) redirects near-real-time consumers
(admission control, post-emit summary, immediate-readback dashboards) to
"polling within the query-latency NFR." That redirect is content-free until
the polled surface's freshness guarantee is written down.

The question this ADR answers: what consistency contract does UC publish to
SDK and REST consumers across the ingestion-ack path and the Query SPI path,
and how strong is the floor that every plugin MUST honour irrespective of
backend?

## Decision Drivers

- **Plugin neutrality** — the floor MUST be achievable by every plugin on the
  v1 roadmap under its default deployment posture; what a plugin can do with
  custom routing or non-default flags is a per-plugin ceiling, not a floor
  ([§3.10](../DESIGN.md#310-consistency-contract) defines the floor; each
  plugin's deployment guide owns its ceiling).
- **Caller actionability** — consumers MUST be able to derive correct
  read-after-write behaviour from the floor alone, without reading any
  per-plugin documentation, even if that means rejecting the Query SPI for
  same-request outcome flows.
- **Structural honesty about the workload-isolation NFR** — the contract MUST
  name the queryability lag the isolated-pool routing implies, instead of
  letting the ingestion-latency NFR carry both meanings.
- **Stability across plugin substitution** — operator-driven plugin swaps
  (TimescaleDB single-node ↔ ClickHouse replicated) MUST NOT break the
  floor consumers code against; any consumer that needs a tighter bound
  consciously couples itself to that plugin's ceiling.
- **No surface bloat on the SPI** — a typed `consistency_profile()` SPI
  method is only worth adding if a real consumer today switches behaviour on
  the profile; prose suffices when no consumer does.

## Considered Options

- **Floor-and-ceiling split (eventual no-bound floor; plugin profile
  ceiling)** — DESIGN publishes a single plugin-agnostic floor: ingestion ack
  is durable and dedup-visible; every Query SPI read (raw, aggregated,
  catalog) is eventually consistent with no upper bound on staleness relative
  to a same-tenant ingestion; per-plugin deployment guides MAY advertise a
  stronger profile (sync single-node, bounded ≤ N ms, etc.), and consumers
  that depend on the stronger bound couple themselves to that plugin.
- **Monotonic-reads-per-`(tenant, usage-type)` floor** — same floor as above plus
  an additional guarantee that once a consumer has observed record R, no
  subsequent read of the same `(tenant_id, gts_id)` returns a state
  that omits R.
- **Bounded-staleness floor (e.g., ≤ N ms across all backends)** — publish a
  single numeric staleness bound the Query SPI MUST honour, and have plugins
  guarantee it under their default deployment.
- **Read-your-writes floor (session-affinity required of every plugin)** —
  publish a floor that requires the Query SPI to reflect a same-caller
  ingestion ack immediately, forcing every plugin to implement session
  affinity or equivalent.

## Decision Outcome

Chosen option: **"Floor-and-ceiling split (eventual no-bound floor; plugin
profile ceiling)"**, because it is the only option where the floor is
achievable by every plugin on the v1 roadmap under default deployment
posture, it states the consequence of `cpt-cf-usage-collector-nfr-workload-isolation`
rather than concealing it, and it lets plugins that can do better advertise a
ceiling without forcing every consumer to defend against the weakest case.
Monotonic-reads is rejected because ClickHouse-replicated's default Distributed
engine routes reads across replicas without session affinity and without
`select_sequential_consistency`, so a consumer observing R against one replica
may legitimately fail to observe R on a subsequent read against another
replica; promising monotonic-reads at the floor would force ClickHouse plugin
authors to either declare non-conformance or require non-default routing,
contradicting the "default deployment posture" criterion. Bounded-staleness
and read-your-writes floors are rejected for the same shape of reason — they
overpromise for backends whose default replication topology cannot meet the
bound without custom configuration.

The floor applies to both `usage_records` and the plugin-owned `usage_type_catalog`
reached through the Plugin SPI.
The floor is per-`(tenant_id, gts_id)`; UC publishes no cross-tenant
or cross-usage-type ordering claim.

A typed `consistency_profile()` SPI method is deferred. The Plugin SPI surface
does not change in v1; per-plugin profile advertisement lives in prose inside
each plugin's deployment guide. The method MAY be added additively under
`cpt-cf-usage-collector-adr-contract-stability` once a real consumer needs to
switch behaviour on the profile.

### Consequences

- DESIGN §3.10 (Consistency Contract) carries the floor and the consumer rules; it ties back to
  `cpt-cf-usage-collector-nfr-workload-isolation` so the contract reads as a
  consequence of an already-allocated NFR, not a fresh policy.
- DESIGN §3.12.7 polling redirect gains concrete content — it now points at
  [§3.10](../DESIGN.md#310-consistency-contract) for the freshness the
  polled Query SPI surface guarantees and the read-after-write constraint
  that follows.
- `plugin-spi.md` gains a **Consistency profile** subsection that restates the
  floor as the SPI's floor and requires every plugin's deployment guide to
  publish its actual profile (sync, bounded, eventual). No SPI method is
  added.
- Read-after-write calling-gear flows (admission control, post-emit summary,
  immediate-readback dashboards) MUST NOT be designed against the Query SPI;
  they MUST use the ingestion ack for same-request outcome. Near-real-time
  observers MUST poll within `cpt-cf-usage-collector-nfr-query-latency` and
  tolerate lag bounded by the active plugin's ceiling. Consumers MAY also
  observe an in-flight record disappear from a later read (no monotonic-reads
  in the floor); flows that require it MUST consume a plugin whose deployment
  guide advertises that ceiling.
- PRD §9 acceptance criterion currently reading "ingestion-latency-bounded
  freshness" is reworded to separate (a) ingestion ack latency bounded by
  `cpt-cf-usage-collector-nfr-ingestion-latency`, from (b) Query SPI
  queryability bounded by the active plugin's published profile with a no-bound
  gear-level floor; this is a PRD wording fix, not a new acceptance gate.
- `sdk-trait.md` and the feature documents inherit the floor through a single
  pointer back to DESIGN [§3.10](../DESIGN.md#310-consistency-contract);
  `features/usage-query.md` calls out the no-read-your-writes constraint in
  its capability summary; `features/usage-emission.md` cross-references it;
  `features/event-deactivation.md` clarifies that the documented atomicity of
  the deactivate cascade is a plugin-transaction invariant, NOT a cross-path
  guarantee against subsequent queries.
- `usage-collector-v1.yaml` is unchanged; the wire format carries no
  read-after-write claim today, so no schema or description edit is required.
- Plugin authors carry an explicit obligation to publish a consistency profile
  in their deployment guide. The floor is small enough that any plugin on the
  v1 roadmap meets it by default; the cost is documentation, not engineering.

### Confirmation

Compliance is confirmed through (a) DESIGN [§3.10](../DESIGN.md#310-consistency-contract)
review by usage-collector maintainers covering the floor wording, the tie to
`cpt-cf-usage-collector-nfr-workload-isolation`, and the §3.12.7 redirect
patch; (b) `plugin-spi.md` review covering the Consistency profile subsection
and the explicit absence of a `consistency_profile()` method in v1; (c)
`cypilot validate` PASS for the usage-collector bundle covering this ADR and
every artifact that back-references it (DESIGN §3.10 and §5 inventory,
PRD §9 wording fix, `plugin-spi.md` subsection, `sdk-trait.md` pointer note,
the three feature pointers); (d) a deployment-guide checklist item in each
active plugin's release readiness review confirming the per-plugin profile is
published.

## Pros and Cons of the Options

### Floor-and-ceiling split (eventual no-bound floor; plugin profile ceiling)

DESIGN publishes a single plugin-agnostic floor: ingestion ack is durable and
dedup-visible; every Query SPI read is eventually consistent with no upper
bound relative to a same-tenant ingestion; per-plugin deployment guides
advertise the actual profile.

- Good, because every plugin on the v1 roadmap honours the floor under
  default deployment posture, so the contract holds across operator-driven
  plugin substitution without requiring custom flags.
- Good, because consumers that need a stronger bound consciously couple to a
  specific plugin's ceiling, making the coupling visible at design review
  rather than hidden as an implicit assumption.
- Good, because read-after-write is forced onto the ingestion ack — the path
  that already returns synchronously and carries the durable outcome — and
  off the Query SPI, which closes a class of latent bugs in
  admission-control and immediate-readback flows.
- Good, because the Plugin SPI surface does not grow; prose profiles in
  deployment guides absorb the variability without forcing every consumer to
  branch.
- Neutral, because the floor is the weakest plugin-neutral guarantee that
  carries real content — consumers must defend against observed-then-disappeared
  records and indeterminate lag, which is an honest cost of plugin neutrality.
- Bad, because consumers reading the floor in isolation may design defensively
  against a worst case their bound plugin would never exhibit, leaving
  potential latency or UX on the table; the mitigation is the per-plugin
  ceiling advertised in the deployment guide.

### Monotonic-reads-per-`(tenant, usage-type)` floor

Same floor as above plus a "once observed, never disappears" guarantee per
`(tenant_id, gts_id)`.

- Good, because consumers do not have to defend against observed-then-disappeared
  records, which is the kind of edge case callers under-handle in practice.
- Good, because TimescaleDB single-node honours it trivially (single primary
  serves reads).
- Bad, because ClickHouse-replicated's default Distributed engine routes
  reads across replicas without session affinity and without
  `select_sequential_consistency`; a consumer reading from replica A and then
  replica B can legitimately observe R on the first read and miss R on the
  second.
- Bad, because forcing the ClickHouse plugin to satisfy this floor at default
  posture either requires per-tenant session affinity (operationally costly
  and capacity-bounded) or `select_sequential_consistency` (which serializes
  reads against replication and breaks the query-latency NFR's performance
  posture under default ClickHouse-replicated deployment).
- Bad, because backends added in the future (any read-replicated SQL backend
  without affinity, any eventually-consistent KV store) would need custom
  routing to conform, raising the cost of plugin pluralism — exactly what
  `cpt-cf-usage-collector-adr-pluggable-storage` is meant to keep open.

### Bounded-staleness floor (e.g., ≤ N ms across all backends)

Publish a numeric staleness bound the Query SPI MUST honour irrespective of
backend.

- Good, because consumers know an exact upper bound on lag and can size
  freshness budgets accordingly.
- Good, because alerting on Query-SPI lag becomes a single threshold rather
  than a per-plugin envelope.
- Bad, because no v1-roadmap plugin can honour an exact `N` without custom
  routing — ClickHouse-replicated replication lag is workload-dependent and
  unbounded under heavy ingestion bursts; TimescaleDB on read replicas has
  the same problem under replica catch-up.
- Bad, because the floor would have to be set very conservatively (seconds,
  perhaps tens of seconds), defeating the freshness budgets it is meant to
  enable; or it would have to be set aspirationally and silently violated
  under load.

### Read-your-writes floor (session-affinity required of every plugin)

Publish a floor that requires the Query SPI to reflect a same-caller
ingestion ack immediately.

- Good, because immediate-readback and admission-control flows can be
  designed against the Query SPI directly.
- Bad, because session affinity is a deployment-substrate concern (operator
  routing, client pinning, sticky load balancing) that the gear cannot
  control; making it a floor would push the entire affinity stack into the
  plugin contract.
- Bad, because the Plugin SPI is fronted by a stateless gateway pool that
  may dispatch the same caller's reads to different plugin connections; the
  floor cannot be made true at the gear boundary, only at the plugin's own
  routing layer.
- Bad, because the obvious workaround — building read-after-write on the
  ingestion ack path that already exists — is strictly simpler and is what
  the floor-and-ceiling option mandates.

## More Information

- DESIGN §1.2 NFR allocation pin `cpt-cf-usage-collector-nfr-workload-isolation`
  to isolated backend pools — the structural source of the queryability lag
  the floor names.
- `cpt-cf-usage-collector-adr-mandatory-idempotency` (ADR-0004) and
  `cpt-cf-usage-collector-adr-monotonic-deactivation` (ADR-0005) already document
  idempotency-by-key and the depth-1 deactivate cascade as plugin-transaction
  invariants; [§3.10](../DESIGN.md#310-consistency-contract) is additive
  and does NOT relitigate either.
- DESIGN §3.11.1 Performance Patterns documents that the gateway dispatches
  `get_usage_type` against the plugin SPI per call; this ADR does not change
  that mechanism.
- DESIGN §3.12.7 Event architecture N/A redirects near-real-time consumers
  to polling within `cpt-cf-usage-collector-nfr-query-latency`; the redirect
  text is updated in the same change set to point at the new floor.
- `plugin-spi.md` Cross-entity invariants section documents the strict
  dedup-key-preservation obligation that the floor cites as part of the
  ingestion-ack guarantee.
- Domain applicability:
  - **ARCH** — addressed (this IS the architectural decision: floor-and-ceiling
    split between DESIGN and per-plugin deployment guides).
  - **DATA** — addressed (clarifies what queryability across `usage_records`
    and the plugin-owned `usage_type_catalog` does and does not promise relative
    to ingestion ack).
  - **PERF** — addressed indirectly (the floor reads as the cost-side of
    `cpt-cf-usage-collector-nfr-workload-isolation`; no new performance
    obligation introduced).
  - **REL** — addressed (the floor is the consistency contract reliability
    consumers code against; pre-existing within-transaction invariants
    survive unchanged).
  - **INT** — addressed (no SPI method added; deployment-guide obligation
    levied on plugin authors).
  - **SEC** — Not applicable: consistency posture does not change the auth
    surface, PDP enforcement, or `SecurityContext` propagation.
  - **OPS** — addressed (each plugin's deployment guide MUST publish its
    consistency profile; consumers reading the floor know whether to design
    against the floor or the plugin ceiling).
  - **MAINT** — addressed (no gear-side migration; documentation-only
    obligation on plugin authors).
  - **TEST** — Not applicable in the ADR body: test design belongs in
    `features/*` (cascade phase) per TEST-ADR-NO-001.
  - **COMPL** — Not applicable: consistency posture is internal substrate
    behaviour, not a regulated-data property.
  - **UX** — Not applicable: no end-user UI; downstream-console freshness
    behaviour is the consumer's responsibility once the floor is published.
  - **BIZ** — Not applicable in the ADR body: requirements belong in PRD per
    BIZ-ADR-NO-001.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses or relates to the following requirements,
decisions, or design elements:

- `cpt-cf-usage-collector-nfr-workload-isolation` — the floor states the
  read-side consequence of the isolated-pool routing this NFR allocates; the
  ADR reads as a follow-through on an allocation already made.
- `cpt-cf-usage-collector-adr-pluggable-storage` — the floor preserves plugin
  pluralism: every roadmap plugin honours the floor under default deployment
  posture, and operator-driven substitution does not break consumers coded
  against the floor.
- `cpt-cf-usage-collector-adr-0012-unified-plugin-catalog-and-gts-id-reference` — the
  floor explicitly covers the plugin-owned `usage_type_catalog` reached through
  the Plugin SPI, alongside `usage_records`; this is a scope clarification,
  not a new contract on the catalog.
- `cpt-cf-usage-collector-adr-mandatory-idempotency` — the floor cites the
  permanent dedup-tuple visibility on the ingestion path as part of the
  Acknowledged guarantee; no change to the idempotency contract itself.
- `cpt-cf-usage-collector-adr-monotonic-deactivation` — the depth-1
  deactivate cascade atomicity remains a plugin-transaction invariant; the
  floor names it as such and clarifies it is NOT a cross-path guarantee
  against the Query SPI.
- `cpt-cf-usage-collector-adr-usage-compensation` — same scope clarification
  as deactivation: compensation cascade atomicity is within-transaction; the
  resulting `SUM` is observable through the Query SPI subject to the floor.
- `cpt-cf-usage-collector-adr-contract-stability` — the absence of a typed
  `consistency_profile()` SPI method in v1 is reversible additively within
  the Plugin SPI major-version contract if a real consumer demand surfaces.
- `cpt-cf-usage-collector-fr-ingestion`, `cpt-cf-usage-collector-fr-query-raw`,
  `cpt-cf-usage-collector-fr-query-aggregation`,
  `cpt-cf-usage-collector-fr-event-deactivation`,
  `cpt-cf-usage-collector-fr-usage-type-existence-and-semantics` —
  these FRs gain a uniform consistency contract that downstream consumers
  can reason about without per-plugin caveats.
