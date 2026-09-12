---
status: accepted
date: 2026-05-31
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-09-09 by Virtuozzo International GmbH

# Consistency contract for usage-collector read/write paths

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Feed snapshot guarantee](#feed-snapshot-guarantee)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Floor-and-ceiling split](#floor-and-ceiling-split)
  - [Monotonic-reads floor](#monotonic-reads-floor)
  - [Bounded-staleness floor](#bounded-staleness-floor)
  - [Read-your-writes floor](#read-your-writes-floor)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-consistency-contract`

## Context and Problem Statement

The gear routes ingestion and query through separate Plugin SPI methods. A
plugin can therefore place them on isolated backend pools, such as read replicas
or separate executor pools. That isolation is the structural source of
queryability lag between the ingestion acknowledgement and a later query.
Acknowledgement latency and queryability are two different mechanisms with two
different bounds, so one combined freshness figure describes neither.

The gear holds two intra-plugin invariants, and neither one bounds that lag. The
dedup identity of an accepted entry stays visible to later ingestion attempts, for as
long as the referenced GTS type's retention keeps that entry. The
at-most-one-invalidation check commits atomically with the entry it admits, in
one backend transaction, because only the store can make that check atomic.
Neither invariant says anything about visibility to a later raw or aggregated
read.

Near-real-time consumers — admission control, post-emit summary, and
immediate-readback dashboards — poll inside the query-latency NFR. Polling is
content-free as an answer until the polled surface carries a published freshness
guarantee.

The question is what consistency contract the gear publishes to SDK and REST
consumers across the acknowledgement path and the query path. It is also how
strong a floor every plugin must honour, whatever its backend.

## Decision Drivers

- Plugin neutrality — every plugin on the v1 roadmap must meet the floor under
  its default deployment posture. What a plugin achieves with custom routing or
  non-default flags is its own ceiling, not the floor.
- Caller actionability — a consumer must derive correct read-after-write
  behaviour from the floor alone, with no per-plugin document. That holds even
  when it means rejecting the query path for a same-request outcome.
- Honesty about workload isolation — the contract must name the queryability lag
  that isolated-pool routing implies
  (`cpt-cf-usage-collector-nfr-workload-isolation`). The ingestion-latency NFR
  must not carry both meanings.
- Stability across plugin substitution — an operator swap between plugins must
  not break the floor a consumer codes against. A consumer that needs a tighter
  bound couples itself deliberately to one plugin's ceiling.
- No SPI surface bloat — a typed profile-advertisement method is worth adding
  only when a real consumer switches behaviour on the profile. Prose suffices
  while none does.

## Considered Options

- Floor-and-ceiling split — the gear publishes one plugin-agnostic floor. An
  acknowledgement is durable and dedup-visible, and every query read is
  eventually consistent with no upper bound on staleness. A plugin's deployment
  guide can advertise a stronger profile.
- Monotonic-reads floor — the same floor, plus one guarantee. Once a consumer
  observes an entry, no later read of the same tenant and GTS type omits it.
- Bounded-staleness floor — one numeric staleness bound that every query read
  honours, and that every plugin guarantees under its default deployment.
- Read-your-writes floor — the query path reflects a same-caller acknowledgement
  immediately, which forces every plugin to implement session affinity or an
  equivalent.

## Decision Outcome

Chosen option: "Floor-and-ceiling split". Every plugin on the v1 roadmap meets
this floor under default deployment posture. It states the read-side consequence
of `cpt-cf-usage-collector-nfr-workload-isolation` instead of concealing it. It
also lets a plugin that does better advertise a ceiling, rather than make every
consumer defend against the weakest case.

Monotonic reads is rejected because ClickHouse-replicated routes reads across
replicas by default, with no session affinity. A consumer can therefore observe
an entry against one replica and miss it against another. A floor that promised
monotonic reads forces ClickHouse plugin authors either to declare
non-conformance or to require non-default routing, which the default-posture
criterion rules out. Bounded staleness and read-your-writes fail the same way.
Both overpromise for a backend whose default replication topology cannot meet
the bound without custom configuration.

The floor covers usage entries reached through the Plugin SPI. GTS type
declarations sit outside it. The gear does not store them, and their propagation
is a property of the Type Resolver's cache rather than of any plugin read path
(`cpt-cf-usage-collector-adr-registry-owned-typing`). The floor is per tenant
and GTS type, and the gear publishes no cross-tenant and no cross-type ordering
claim.

A typed profile-advertisement method on the SPI is deferred. The SPI surface
does not change in v1, and each plugin's deployment guide carries its profile in
prose. The method can be added additively under
`cpt-cf-usage-collector-adr-contract-stability` once a real consumer needs to
switch behaviour on it.

### Feed snapshot guarantee

The usage feed adds one guarantee the floor does not make. A paginated feed scan
observes a snapshot, because no accepted entry is ever rewritten. The
append-only ledger purchases that property
(`cpt-cf-usage-collector-principle-append-only-ledger`).

Append-only arrivals are the one carve-out. A later page can carry entries
accepted after the scan began, and the watermark returned with each page
demarcates them.

This is a gear-level property, distinct from feed freshness
(`cpt-cf-usage-collector-nfr-billing-feed-freshness`), which stays a per-plugin
readiness gate. A status flip on a delivered row is a mutation, and a scan can
observe it. That is one reason a correction is appended rather than applied in
place.

### Consequences

- A read-after-write flow must not be designed against the query path.
  Admission control, post-emit summary, and immediate readback all take their
  same-request outcome from the ingestion acknowledgement.
- A near-real-time observer polls inside
  `cpt-cf-usage-collector-nfr-query-latency` and tolerates lag bounded by the
  active plugin's ceiling.
- A consumer can observe an in-flight entry and then miss it on a later read,
  because the floor carries no monotonic-reads guarantee. A flow that needs
  monotonic reads must run against a plugin whose deployment guide advertises
  that ceiling.
- The feed is the deliberate exception, and the feed snapshot guarantee above
  states what it adds.
- Acknowledgement latency and queryability are bounded separately.
  `cpt-cf-usage-collector-nfr-ingestion-latency` bounds the acknowledgement, and
  the active plugin's published profile bounds queryability over a gear-level
  floor with no bound.
- Every plugin author must publish a consistency profile in the deployment
  guide. The floor is small enough that any v1-roadmap plugin meets it by
  default, so the cost is documentation rather than engineering.
- The floor is decided once, in
  `cpt-cf-usage-collector-design-consistency-contract`. Every other document
  restates it without re-deciding it.

### Confirmation

- Design review of the floor wording in
  `cpt-cf-usage-collector-design-consistency-contract`, including its tie to
  `cpt-cf-usage-collector-nfr-workload-isolation`.
- Review that no profile-advertisement method appears on the Plugin SPI in v1.
- A deployment-guide checklist item in each active plugin's release-readiness
  review, to show that the per-plugin profile is published.

## Pros and Cons of the Options

### Floor-and-ceiling split

The gear publishes one plugin-agnostic floor. An acknowledgement is durable and
dedup-visible, every query read is eventually consistent with no upper bound,
and each plugin's deployment guide advertises its actual profile.

- Good, because every v1-roadmap plugin honours the floor under default
  deployment posture. The contract therefore survives an operator-driven plugin
  swap with no custom flags.
- Good, because a consumer that needs a stronger bound couples deliberately to
  one plugin's ceiling. That coupling is visible at design review rather than
  hidden as an assumption.
- Good, because read-after-write moves onto the acknowledgement path, which
  already returns synchronously and carries the durable outcome. That closes a
  class of latent bugs in admission-control and immediate-readback flows.
- Good, because the SPI surface does not grow. Prose profiles absorb the
  variability, and no consumer has to branch.
- Neutral, because the floor is the weakest plugin-neutral guarantee that still
  carries content. A consumer must defend against indeterminate lag, and against
  an entry it observed once and then misses.
- Bad, because a consumer reading the floor alone can over-defend against a
  worst case its bound plugin never exhibits. The per-plugin ceiling in the
  deployment guide is the mitigation.

### Monotonic-reads floor

The same floor, plus a "once observed, never disappears" guarantee per tenant
and GTS type.

- Good, because a consumer does not have to defend against an entry that
  disappears from a later read, which callers under-handle in practice.
- Good, because TimescaleDB single-node honours it trivially, since one primary
  serves every read.
- Bad, because ClickHouse-replicated routes reads across replicas by default,
  with no session affinity and no `select_sequential_consistency`. A consumer
  reading replica A and then replica B can legitimately see an entry once and
  miss it next.
- Bad, because this floor at default posture needs one of two costly measures.
  Per-tenant session affinity is operationally costly and capacity-bounded.
  Sequential-consistency reads serialize against replication and break the
  query-latency posture.
- Bad, because a future backend without affinity — any read-replicated SQL
  store, any eventually-consistent KV store — needs custom routing to conform.
  That raises the cost of the plugin pluralism
  `cpt-cf-usage-collector-adr-pluggable-storage` exists to keep open.

### Bounded-staleness floor

One numeric staleness bound that every query read honours, whatever the backend.

- Good, because a consumer knows an exact upper bound on lag and sizes freshness
  budgets against it.
- Good, because alerting on query lag becomes one threshold rather than a
  per-plugin envelope.
- Bad, because no v1-roadmap plugin honours an exact bound without custom
  routing. ClickHouse-replicated replication lag is workload-dependent and
  unbounded under an ingestion burst, and TimescaleDB read replicas have the
  same problem during catch-up.
- Bad, because the bound then has to be very conservative, in seconds or tens of
  seconds. That defeats the freshness budgets it exists to enable. The
  alternative is an aspirational bound, silently violated under load.

### Read-your-writes floor

The query path reflects a same-caller acknowledgement immediately.

- Good, because immediate-readback and admission-control flows can read the
  query path directly.
- Bad, because session affinity is a deployment-substrate concern — operator
  routing, client pinning, sticky load balancing — that the gear does not
  control. Making it a floor pushes the whole affinity stack into the plugin
  contract.
- Bad, because a stateless gateway pool fronts the Plugin SPI and can dispatch
  one caller's reads to different plugin connections. The floor cannot be made
  true at the gear boundary, only inside the plugin's own routing layer.
- Bad, because the obvious workaround is strictly simpler: build read-after-write
  on the acknowledgement path that already exists. That is what the
  floor-and-ceiling option mandates.

## More Information

- `cpt-cf-usage-collector-nfr-workload-isolation` is allocated to isolated
  backend pools, which is the structural source of the queryability lag this
  floor names.
- `cpt-cf-usage-collector-adr-mandatory-idempotency` and
  `cpt-cf-usage-collector-adr-append-only-invalidation` own the two
  plugin-transaction invariants. Both hold inside one store transaction and
  neither is a cross-path guarantee against the query path. This decision is
  additive and re-litigates neither.
- Type resolution reaches `types-registry` through the Type Resolver's cache and
  never reaches a plugin. This decision does not change that.
- The usage feed is deliberately pull-based rather than a push channel, so a
  near-real-time observer polls.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses or relates to the following requirements,
decisions, or design elements:

- `cpt-cf-usage-collector-design-consistency-contract` — the design element that
  carries the floor this decision makes.
- `cpt-cf-usage-collector-nfr-workload-isolation` — the floor states the
  read-side consequence of the isolated-pool routing this NFR allocates.
- `cpt-cf-usage-collector-nfr-query-freshness` — the freshness question that
  this decision answers.
- `cpt-cf-usage-collector-adr-pluggable-storage` — the floor preserves plugin
  pluralism. Every roadmap plugin honours it under default posture, so an
  operator swap does not break a consumer coded against the floor.
- `cpt-cf-usage-collector-adr-registry-owned-typing` — GTS type declarations sit
  outside the floor. The Type Resolver's cache carries their propagation.
- `cpt-cf-usage-collector-adr-mandatory-idempotency` — the floor cites
  dedup-identity visibility as part of the acknowledgement guarantee. That
  visibility lasts as long as retention keeps the entry, and the idempotency
  contract itself is unchanged.
- `cpt-cf-usage-collector-adr-append-only-invalidation` —
  at-most-one-invalidation atomicity stays a plugin-transaction invariant. The
  floor names it as such and states that it is not a cross-path guarantee.
- `cpt-cf-usage-collector-adr-contract-stability` — the absence of a
  profile-advertisement method in v1 is reversible additively inside the Plugin
  SPI major-version contract.
- `cpt-cf-usage-collector-fr-ingestion`,
  `cpt-cf-usage-collector-fr-query-raw`,
  `cpt-cf-usage-collector-fr-query-aggregation`,
  `cpt-cf-usage-collector-fr-record-invalidation`, and
  `cpt-cf-usage-collector-fr-billing-usage-feed` — these requirements gain one
  consistency contract that a downstream consumer can reason about without
  per-plugin caveats.
