---
status: accepted
date: 2026-05-24
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-07-06 by Virtuozzo International GmbH

# PDP-centric authorization for Usage Collector

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [PDP-centric authorization](#pdp-centric-authorization)
  - [In-collector ACL cache](#in-collector-acl-cache)
  - [Hybrid model](#hybrid-model)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-pdp-centric-authorization`

## Context and Problem Statement

The Usage Collector exposes ingestion, query, deactivation, and usage-type-lifecycle operations across three surfaces (REST API, in-process SDK trait, and Plugin SPI), and every operation must be authorized against the caller's SecurityContext plus the operation's full attribution tuple (tenant, resource, UsageType, and optionally subject); the calling gear's identity is supplied intrinsically by the `SecurityContext` and is read by the PDP from there rather than as a separate caller-supplied attribution field. The platform already provides a centralized Policy Decision Point (PDP) called `authz-resolver`; the question is whether the collector should anchor all authorization at the PDP, maintain its own per-tenant or per-UsageType access table, or hybridize the two. Centralized metering is shared across many emitters and consumers, so authorization placement directly affects how access policy evolves and how rapidly drift can creep in between policy declaration and policy enforcement.

## Decision Drivers

- `cpt-cf-usage-collector-fr-ingestion-authorization` — a single PDP check per record against the full attribution tuple is required before plugin dispatch.
- `cpt-cf-usage-collector-fr-tenant-isolation` — tenant isolation must be enforced through PDP authorization rather than per-tenant trust inside the collector.
- PRD §1.3 centralized-metering goal — keep policy declaration and enforcement in one platform-owned location to avoid divergence between calling gears and the metering substrate.

## Considered Options

- PDP-centric authorization — every operation delegates the authorization decision to `authz-resolver` and applies PDP-returned constraint filters to queries before plugin dispatch; the collector keeps no access state.
- In-collector ACL cache — the collector maintains its own per-tenant and per-UsageType access table, refreshed periodically from a platform source of truth, and evaluates authorization locally.
- Hybrid model — PDP for slow-changing access decisions (e.g., UsageType registration, deactivation) plus a short-lived in-collector PDP-decision cache for hot ingestion paths.

## Decision Outcome

Chosen option: "PDP-centric authorization", because it is the only option that satisfies the fail-closed posture and authorization NFRs without re-introducing a parallel access store inside the metering substrate. Every ingestion, query, deactivation, and UsageType lifecycle operation runs a single PDP check, and the PDP-returned constraints are applied to queries before the active plugin executes them. The collector deliberately keeps no per-tenant or per-UsageType access state and no PDP-decision cache, so policy evolution in the platform takes effect immediately and there is no drift surface to manage.

PDP enforcement is performed **per domain component**, not via a centralized adapter. Each domain component — ingestion-gateway, query-gateway, deactivation-handler, and usage-type-catalog — accepts `ctx: &SecurityContext` as the first parameter on every operation and calls `authz-resolver-sdk`'s `PolicyEnforcer::access_scope_with(ctx, ...)` inline through a thin shared helper. The shared `access_scope_with` helper collapses the per-service authorization surface into a single definition site while keeping the call site local to each domain component. The collector defines this helper once and reuses it across its four domain components.

Authentication is owned upstream by the ToolKit gateway (REST surface) via `OperationBuilder::authenticated()` or supplied directly by the in-process caller (SDK trait surface). The collector accepts only a pre-resolved `SecurityContext` and never synthesizes identity, never resolves credentials, and never consumes a platform AuthN contract. Any operation arriving without a resolved `SecurityContext` is rejected at the surface boundary before any domain component runs.

### Consequences

- The collector inherits PDP availability as a hard dependency on the ingestion hot path; outages of `authz-resolver` translate to deterministic ingestion rejection rather than degraded admission.
- Policy changes (new tenants, new calling gears, new UsageType grants) propagate without any collector-side reconfiguration or cache invalidation.
- The single-PDP-check-per-record budget is on the synchronous ingestion path and is explicitly accounted for inside `cpt-cf-usage-collector-nfr-ingestion-latency` (≤ 200 ms p95).
- Query result scoping is uniformly driven by PDP-returned constraint filters; user-supplied filters can only narrow within the authorized scope.
- The collector cannot independently authorize an emission or read; if the PDP is unreachable, the operation fails closed (no shadow allow path, no synthesized identity, no degraded mode).
- Locality: PDP enforcement lives next to the operation it guards inside each domain component. Each component's helper invocation is reviewable in isolation alongside the business logic it gates.
- Helper-consistency obligation: because the call is inlined per component rather than funneled through a single adapter, the shared helper signature (resource type, action verb, owner tenant, optional resource id, and a `require_constraints` flag — `false` on the platform-global catalog path, `true` on the tenant-scoped usage-record and list paths, which additionally gate the returned scope against the operation's attribution) must be maintained consistently across ingestion-gateway, query-gateway, deactivation-handler, and usage-type-catalog. Drift in the helper surface — or any component bypassing the helper — would weaken the uniformity guarantee that a centralized adapter would have made structural.
- Authentication boundary is external: because the collector accepts only a pre-resolved `SecurityContext`, its readiness model does not include an AuthN-client readiness fact. Only PDP-client readiness (`authz-resolver`) is probed at startup, simplifying the collector's failure surface but pushing the entire AuthN failure mode to the ToolKit gateway upstream.

### Confirmation

Compliance is confirmed through (a) design review against the §3.2 component model showing every domain component (ingestion-gateway, query-gateway, deactivation-handler, usage-type-catalog) routes through the per-service `access_scope_with` helper before plugin dispatch, (b) authorization conformance tests covering permit / deny / constraint-filtered scenarios on every operation, and (c) negative tests confirming PDP unavailability surfaces as deterministic failure rather than fallback admission.

## Pros and Cons of the Options

### PDP-centric authorization

The collector has no access state of its own; every decision is made by `authz-resolver` against the live policy graph.

- Good, because policy evolves in one place and propagates immediately to every metering operation.
- Good, because it matches the fail-closed posture by removing every fallback path that could mask a denied operation.
- Good, because it eliminates the long-running cache-coherence work that a local ACL table would create.
- Neutral, because every authorized operation incurs a PDP round-trip, which is acceptable inside the ingestion latency budget but must be measured.
- Bad, because PDP unavailability is a hard dependency on the synchronous ingestion path; mitigations live in the platform's PDP availability story rather than inside the collector.

### In-collector ACL cache

The collector maintains its own per-tenant and per-UsageType access table, refreshed from a platform source of truth and evaluated locally on each request.

- Good, because ingestion does not block on PDP availability and the per-record authorization cost stays in-process.
- Bad, because it duplicates the platform's authorization state and re-creates the drift surface the centralized PDP exists to eliminate.
- Bad, because cache refresh policy, invalidation, and audit become collector responsibilities even though the collector has no business owning policy.
- Bad, because a stale cache can produce silent over-permissive decisions, which directly violates the fail-closed posture.

### Hybrid model

A PDP-decision cache (e.g., 30-second TTL) on the hot ingestion path; slow-change operations (usage-type registration, deactivation) still call the PDP synchronously.

- Good, because it preserves PDP authority for low-frequency operations while smoothing PDP load for ingestion.
- Bad, because any positive-cache TTL re-introduces the very drift surface the centralized PDP is meant to eliminate.
- Bad, because cache coherence and revocation semantics become a collector responsibility under a clear "no business logic" constraint (`cpt-cf-usage-collector-constraint-no-business-logic`).
- Neutral, because the same outcome (lower PDP load) is better achieved by platform-side PDP scaling than by collector-side caching.

## More Information

Related decisions: `cpt-cf-usage-collector-adr-caller-supplied-attribution` (the attribution tuple that this PDP check authorizes); `cpt-cf-usage-collector-adr-mandatory-idempotency` (which keeps retry-on-PDP-failure semantics safe). The PDP and AuthN contracts are platform-level and are not redefined by this decision.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-ingestion-authorization` — one PDP check per record against the full attribution tuple before plugin dispatch.
- `cpt-cf-usage-collector-fr-tenant-isolation` — tenant isolation is realized exclusively through PDP authorization and PDP-returned constraint filters.
- `cpt-cf-usage-collector-principle-pdp-centric-authorization` — codifies the principle in §2.1.
- `cpt-cf-usage-collector-principle-fail-closed` — operationalizes the fail-closed posture on the authorization path.
