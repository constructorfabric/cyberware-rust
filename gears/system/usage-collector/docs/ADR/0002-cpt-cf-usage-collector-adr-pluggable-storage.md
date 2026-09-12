---
status: accepted
date: 2026-05-24
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-09-09 by Virtuozzo International GmbH

# Pluggable storage via Plugin SPI for Usage Collector

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Scope](#scope)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Storage plugin behind an SPI](#storage-plugin-behind-an-spi)
  - [Embedded single backend](#embedded-single-backend)
  - [Compiled-in driver registry](#compiled-in-driver-registry)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-pluggable-storage`

## Context and Problem Statement

Two workload profiles pull the gear in different directions. Ingestion must
sustain at least 10,000 **Usage Records** per second. A 30-day single-tenant
aggregation must finish within 500 ms at p95. Different storage technologies suit
each profile: columnar engines for analytical reads, time-series engines for
retention-tuned writes.

One implementation must serve every deployment, and no operator can be locked
into one backend. Three routes are open. The gear can embed one chosen backend,
reach every backend through an internal abstraction, or reach storage only
through the platform's plugin mechanism.

## Decision Drivers

- `cpt-cf-usage-collector-fr-pluggable-storage` — persistence and query reach the
  backend only through a dedicated Plugin SPI.
- `cpt-cf-usage-collector-nfr-query-latency` — a 30-day single-tenant
  aggregation finishes within 500 ms at p95.
- `cpt-cf-usage-collector-nfr-throughput` — ingestion sustains at least 10,000
  **Usage Records** per second.
- `cpt-cf-usage-collector-nfr-workload-isolation` — query load must not degrade
  ingestion p95 latency.
- `cpt-cf-usage-collector-constraint-vendor-pluggable` — no storage-vendor
  lock-in and no licensing assumption inside the gear.
- `cpt-cf-usage-collector-nfr-plugin-contract-stability` — the SPI surface stays
  stable across plugin churn, so plugin authors and the gear release
  independently.
- The **Centralized metering** goal in [PRD.md](../PRD.md) — an operator fits the
  backend to the workload without a coordinated gear release.

## Considered Options

- Storage plugin behind an SPI — the gear reaches persistence and query only
  through a dedicated Plugin SPI. Operator configuration selects one plugin per
  GTS instance, and the gear resolves the binding lazily on the first dispatch.
- Embedded single backend — the gear couples directly to one technology, for
  example ClickHouse. Another backend needs a fork.
- Compiled-in driver registry — the gear ships several drivers and a
  configuration switch, and uses no platform plugin mechanism.

## Decision Outcome

Chosen option: "Storage plugin behind an SPI". It is the only option that meets
the pluggable-storage requirement and both performance NFRs. The gear takes no
position on any backend's schema, dialect, or client library.

The Plugin SPI is the single seam between the gear and storage. Nothing reaches
persistence or query by another route. Operator configuration selects one plugin
identity per GTS instance. The gear resolves the active binding lazily, on the
first dispatch after `types-registry` is consistent. Binding is decentralized: no
orchestrator component owns that lifecycle.
`cpt-cf-usage-collector-principle-plugin-resolution-via-client-hub` holds the
mechanics.

Each plugin ships its own implementation, deployment guide, and operational
runbook, and releases on its own schedule.

### Scope

The seam covers usage entries and the reads over them. It does not cover GTS
type declarations. `types-registry` owns those, and they reach the gear through
the Type Resolver (`cpt-cf-usage-collector-adr-registry-owned-typing`). A storage
plugin never persists a declaration, never serves one, and never enforces
referential integrity against one.
`cpt-cf-usage-collector-constraint-no-type-catalog` states that boundary.

### Consequences

- The ingestion and query paths use SPI types only. They hold no backend SQL, no
  schema, and no client library code.
- An operator selects the active plugin by configuration, per GTS instance. A
  backend change needs no Usage Collector release.
- Plugin authors own every performance-shaping choice: pre-aggregated views,
  columnar indexes, partition strategy, retention tiering, backup, and
  point-in-time recovery. Each choice must meet the platform NFR thresholds.
- The ingestion path depends on the PDP, on `types-registry` for type
  resolution, and on the active plugin binding. Consumer availability does not
  enter it.
- `cpt-cf-usage-collector-adr-contract-stability` governs the SPI itself. A
  breaking change needs a coordinated multi-major release.

### Confirmation

- Design review of the plugin-ownership boundary, to show that no SQL, no schema,
  and no backend client code lives in the gear.
- Contract conformance tests against the published Plugin SPI, for every plugin
  that ships with the platform.
- NFR load tests for query latency, ingestion throughput, and workload
  isolation, against each supported backend.

## Pros and Cons of the Options

### Storage plugin behind an SPI

Operator configuration selects the backend. The gear reaches it only through the
platform's plugin mechanism, and resolves the binding lazily on the first
dispatch.

- Good, because the gear carries no backend dependency, and an operator meets the
  NFR thresholds with the technology that fits the workload.
- Good, because plugin authors and the gear release independently under the
  major-version stability contract.
- Good, because the binding lifecycle is already a platform-wide pattern with
  operational tooling.
- Neutral, because every persistence and query call crosses the SPI boundary. The
  SPI is in-process and accepts batched operations, so this cost is acceptable.
- Bad, because the platform NFR thresholds become a per-plugin obligation. The
  gear asserts them through conformance tests and cannot enforce them
  structurally.

### Embedded single backend

The gear couples directly to one storage technology. An operator cannot select
another one.

- Good, because the gear optimizes tightly against one schema and one client, and
  pays no SPI translation cost.
- Bad, because an operator with a different scale or compliance profile must fork
  the gear.
- Bad, because the gear's release cycle follows the backend's release cycle.
- Bad, because contract stability for plugin authors does not apply, and the
  platform has no backend-agnostic guarantee.

### Compiled-in driver registry

The gear ships several drivers and a configuration switch. It bypasses the
platform plugin mechanism.

- Good, because driver selection is simple and needs no platform binding
  machinery.
- Bad, because a new backend needs a gear release, which restores the coupling
  that the SPI exists to remove.
- Bad, because an operator cannot take a plugin-author release on its own. Every
  backend update needs a gear rebuild.
- Bad, because it diverges from platform convention and creates a one-off
  persistence pattern.

## More Information

`cpt-cf-usage-collector-adr-contract-stability` governs Plugin SPI versioning.
`cpt-cf-usage-collector-principle-pluggable-storage` and
`cpt-cf-usage-collector-principle-plugin-resolution-via-client-hub` carry the
design-level statement of this decision, including the binding mechanics that
this ADR does not restate.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-pluggable-storage` — the Plugin SPI is the only
  persistence and query seam.
- `cpt-cf-usage-collector-nfr-query-latency` — backend-native acceleration meets
  the 500 ms p95 aggregation budget.
- `cpt-cf-usage-collector-nfr-throughput` — backend-native bulk-write paths
  deliver at least 10,000 **Usage Records** per second.
- `cpt-cf-usage-collector-nfr-workload-isolation` — separate SPI methods for
  ingestion and query let a plugin route to isolated backend pools.
- `cpt-cf-usage-collector-principle-pluggable-storage` — the design principle
  that this decision codifies.
- `cpt-cf-usage-collector-constraint-vendor-pluggable` — the no-lock-in
  constraint that the seam realizes.
- `cpt-cf-usage-collector-constraint-plugin-contract-stability` — pairs with this
  ADR to govern Plugin SPI evolution.
- `cpt-cf-usage-collector-interface-plugin` and
  `cpt-cf-usage-collector-contract-storage-plugin` — the SPI interface and
  contract that this decision realizes.
- `cpt-cf-usage-collector-adr-registry-owned-typing` — narrows this ADR's scope
  back to usage entries.
- `cpt-cf-usage-collector-constraint-no-type-catalog` — the constraint that this
  scope boundary realizes.
