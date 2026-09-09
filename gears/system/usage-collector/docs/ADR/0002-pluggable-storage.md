---
status: accepted
date: 2026-05-24
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-06-10 by Virtuozzo International GmbH

# Pluggable storage via Plugin SPI for Usage Collector

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Plugin SPI binding via Plugin Host + GTS Registry](#plugin-spi-binding-via-plugin-host--gts-registry)
  - [Embedded single backend](#embedded-single-backend)
  - [In-process driver registry without SPI](#in-process-driver-registry-without-spi)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-pluggable-storage`

## Context and Problem Statement

The Usage Collector must support sustained ingestion of at least 10,000 records/sec while delivering 30-day single-tenant aggregations at ≤ 500 ms p95, and these workload profiles map naturally onto different storage technologies (e.g., columnar OLAP for analytical reads, time-series engines for retention-tuned writes). The platform requires that a single core implementation serve every deployment without locking operators into one specific backend. The question is whether the core should embed a chosen backend directly, reach every backend through a single in-process abstraction, or use the platform's Plugin SPI mechanism so the active backend is resolved lazily on the first dispatch after the `types-registry` is consistent.

## Decision Drivers

- `cpt-cf-usage-collector-nfr-query-latency` — aggregation queries over 30-day single-tenant ranges must complete within 500 ms p95.
- `cpt-cf-usage-collector-nfr-throughput` — sustained ingestion of ≥ 10,000 records/sec.
- `cpt-cf-usage-collector-fr-pluggable-storage` — persistence and query are reached through a dedicated Plugin SPI resolved lazily on the first dispatch via the Plugin Host (the host gear's own Service) and the GTS Registry.
- `cpt-cf-usage-collector-nfr-workload-isolation` — query workloads must not degrade ingestion p95 latency.
- PRD §1.3 backend-agnostic objective (centralized metering goal) — operator-selected backends fit workload profile without coordinated collector releases.
- `cpt-cf-usage-collector-nfr-plugin-contract-stability` (PRD §6.1) — surface stability must hold across plugin churn so plugin authors and the core release independently.

## Considered Options

- Plugin SPI binding via Plugin Host + GTS Registry — operator configuration (`[usage_collector].vendor` read once at `Gear::init`) selects one plugin identity per GTS instance; the host's `GtsPluginSelector` resolves the bound instance lazily on the first dispatch after the `types-registry` is consistent; the core reaches persistence and query only through the Plugin SPI.
- Embedded single backend — the core directly couples to one chosen technology (e.g., ClickHouse) with internal abstraction; alternative backends are forks or branches.
- In-process driver registry without SPI — the core ships with several drivers compiled in and exposes a configuration switch but uses no platform-level SPI mechanism.

## Decision Outcome

Chosen option: "Plugin SPI binding via Plugin Host + GTS Registry", because it satisfies the pluggable-storage FR and the query/ingestion NFRs simultaneously without forcing the core to take a position on any specific backend's schema, dialect, or client library. The Plugin SPI is the single seam between the core and persistence/query; the active backend is resolved lazily on the first dispatch after the `types-registry` is consistent and reachable only through SPI methods. There is no separate "Gear Orchestrator" component — binding is decentralised across the host gear's `Service` constructor (which materializes the `GtsPluginSelector`) and each plugin gear's own `init()` (which performs the scoped `ClientHub::register_scoped`). Each plugin ships its own implementation, deployment guide, and operational runbook on an independent release schedule from the Usage Collector.

### Consequences

- The core ingestion and query paths are written against Plugin SPI types only and contain no backend-specific SQL, schema, or client library code.
- Operators select the active plugin via configuration per GTS instance; switching backends does not require a Usage Collector release.
- Plugin authors are responsible for performance-shaping decisions (pre-aggregated materialized views, columnar indexes, partition strategies, retention tiering, durable backup, point-in-time recovery) that meet the platform NFR thresholds.
- The core depends only on AuthN, PDP, the local UsageType catalog, and the active plugin binding for ingestion; downstream consumer availability does not enter the ingestion path.
- The Plugin SPI is itself bound by the contract-stability ADR (`cpt-cf-usage-collector-adr-contract-stability`); breaking changes require a coordinated multi-major release.

Scope clarification (added 2026-05-30, carried by `cpt-cf-usage-collector-adr-0012-unified-plugin-catalog-and-gts-id-reference`): the pluggable-storage scope explicitly covers the usage-type catalog alongside `usage_records`. The catalog physically lives on the storage plugin's backend database next to `usage_records` and is reached only through the Plugin SPI; an `ON DELETE RESTRICT` foreign key between `usage_records` and the catalog enforces referential integrity natively. The "Plugin SPI binding via Plugin Host + GTS Registry" decision and its rationale documented above apply uniformly to the catalog at the same seam.

### Confirmation

Compliance is confirmed through (a) design review of the plugin-ownership boundary in DESIGN §3.7 and `plugin-spi.md` §"Cross-entity invariants honored by the Plugin SPI" confirming that no SQL, schema, or backend-specific client code lives in the core, (b) contract conformance tests against the published Plugin SPI for every plugin shipped with the platform, and (c) NFR load tests (query latency, ingestion throughput, workload isolation) executed against each supported backend.

## Pros and Cons of the Options

### Plugin SPI binding via Plugin Host + GTS Registry

The active backend is selected by operator configuration (`[usage_collector].vendor` read once at `Gear::init`) and reached only through the platform's Plugin SPI mechanism, with binding lifecycle decentralised across the host gear's `Service` constructor (which materializes the `GtsPluginSelector`) and each plugin gear's own `init()` (which performs the scoped `ClientHub::register_scoped`); the active instance is resolved lazily on the first dispatch via the GTS Registry. There is no separate "Gear Orchestrator" component.

- Good, because it removes every backend-specific dependency from the core and lets operators meet NFR thresholds with the technology that fits their workload.
- Good, because plugin authors and the core release independently under the major-version stability contract.
- Good, because the binding lifecycle (registration, instantiation, hot replacement window) is already a platform-wide pattern with operational tooling.
- Neutral, because every persistence and query call crosses the SPI boundary; this is acceptable since the SPI is in-process and shaped to accept batched operations.
- Bad, because the platform's NFR thresholds become a per-plugin obligation that the core can only assert through conformance tests, not enforce structurally.

### Embedded single backend

The core directly couples to one chosen storage technology with internal abstractions but no operator-selectable plugin model.

- Good, because the core can optimize tightly against one backend's schema and client and avoid SPI translation cost.
- Bad, because operators with different scale or compliance profiles cannot select an alternative backend without forking the collector.
- Bad, because the core release cycle becomes coupled to the chosen backend's release cycle.
- Bad, because contract stability for plugin authors no longer applies, and the platform loses the backend-agnostic guarantee from PRD §1.3.

### In-process driver registry without SPI

The core ships with several drivers compiled in and exposes a configuration switch but bypasses the platform's Plugin SPI / GTS Registry mechanism.

- Good, because driver selection is simple and avoids platform-wide binding machinery.
- Bad, because adding a new backend requires a core release, reintroducing the very coupling the SPI exists to remove.
- Bad, because operators cannot run plugin-author releases independently; every backend update demands a core rebuild.
- Bad, because it bypasses platform conventions (Plugin Host + GTS Registry + ClientHub) and creates a one-off persistence pattern that diverges from other gears.

## More Information

Related decisions: `cpt-cf-usage-collector-adr-contract-stability` (which governs Plugin SPI versioning). The DESIGN §3.7 stateless-gateway statement, `plugin-spi.md` §"Cross-entity invariants honored by the Plugin SPI", and the Plugin SPI seam in §3.2 are the structural anchors for this decision.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-pluggable-storage` — Plugin SPI as the only persistence/query seam.
- `cpt-cf-usage-collector-nfr-query-latency` — backend-native acceleration meets the 500 ms p95 aggregation budget.
- `cpt-cf-usage-collector-nfr-throughput` — backend-native bulk-write paths deliver ≥ 10,000 records/sec.
- `cpt-cf-usage-collector-nfr-workload-isolation` — independent SPI methods for ingestion and query let plugins route to isolated backend pools.
- `cpt-cf-usage-collector-principle-pluggable-storage` — codifies the principle in §2.1.
- `cpt-cf-usage-collector-constraint-plugin-contract-stability` — pairs with this ADR to govern Plugin SPI evolution.
- `cpt-cf-usage-collector-interface-plugin` and `cpt-cf-usage-collector-contract-storage-plugin` — the SPI interface and contract realized by this decision.
- `cpt-cf-usage-collector-adr-0012-unified-plugin-catalog-and-gts-id-reference` — re-expands this ADR's scope to include the usage-type catalog alongside `usage_records` and adds the FK-enforced referential delete invariant; this ADR's original "Plugin SPI binding via Plugin Host + GTS Registry" decision and rationale are unchanged.
