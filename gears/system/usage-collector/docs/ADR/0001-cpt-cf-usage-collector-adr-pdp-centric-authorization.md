---
status: accepted
date: 2026-05-24
---

Created:  2026-05-22 by Virtuozzo International GmbH
Updated:  2026-09-09 by Virtuozzo International GmbH

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

Every Usage Collector operation must be authorized: ingestion, invalidation,
query, aggregation, and feed read. Each check needs the caller's resolved
security context and the operation's full attribution tuple (tenant, resource,
GTS type, and optional subject). The calling gear's
identity comes from the security context, not from a separate attribution field.

The platform already provides a centralized Policy Decision Point (PDP),
`authz-resolver`. The question is where the gear anchors these checks. Many
emitters and many consumers share one metering gear, so the answer determines
how policy evolves and how fast declaration and enforcement drift apart.

## Decision Drivers

- `cpt-cf-usage-collector-fr-ingestion-authorization` — every entry is
  authorized against its full attribution tuple before the gear persists it.
- `cpt-cf-usage-collector-fr-tenant-isolation` — authorization enforces tenant
  isolation. Per-tenant trust inside the gear does not.
- `cpt-cf-usage-collector-principle-fail-closed` — no path admits an operation
  that the PDP does not permit.
- The **Centralized metering** goal in [PRD.md](../PRD.md) — one authoritative
  store for every service that measures usage. Policy declaration and policy
  enforcement therefore stay in one platform-owned place.

## Considered Options

- PDP-centric authorization — every operation delegates the check to
  `authz-resolver`. The gear applies the returned constraints to reads and keeps
  no access state.
- In-collector ACL cache — the gear keeps a per-tenant and per-GTS-type access
  table, refreshed from a platform source of truth, and decides locally.
- Hybrid model — the PDP handles low-frequency operations. A short-lived cache
  of PDP results covers the high-rate ingestion path.

## Decision Outcome

Chosen option: "PDP-centric authorization". It is the only option that holds the
fail-closed posture without a second access store inside the gear.

Both inbound surfaces — the REST API and the in-process SDK trait — carry the
same authorization. No operation reaches the storage plugin without a PDP
decision that covers it, and the returned constraints scope that operation. The
gear keeps no access state and caches no PDP decision, so it holds nothing that
can disagree with platform policy. How fast a policy change reaches a decision
is a property of the PDP, not of this gear.

Every guarded operation has one shape. Ingestion, query, and feed operations
are all tenant-scoped. The PDP returns the scope the caller may act in. The
gear then checks the operation's attribution against that scope and denies
anything outside it. Both parts are necessary: the PDP decides the scope, and
this check holds each operation inside it.

A correction is not a second shape. The gear admits it as an ordinary appended
entry, through the same check as the measurement it withdraws. A feed
subscription is scoped like any other read, so it cannot widen beyond what the
PDP returns.

Each domain component that owns a guarded operation runs the check itself,
through one shared helper. There is no centralized adapter and no framework
middleware. This placement keeps each check next to the operation it guards, and
keeps in-process and REST callers on one authorization path.

Authentication is outside this decision and outside the gear. The ToolKit
gateway resolves it on the REST surface, and the caller supplies it on the SDK
surface. The gear accepts only a pre-resolved security context. It never
synthesizes identity, never resolves credentials, and consumes no platform
authentication contract. The surface boundary rejects an operation that arrives
without a resolved security context, before any domain component runs.

### Consequences

- PDP availability becomes a hard dependency on the ingestion path. An
  `authz-resolver` outage produces deterministic rejection, not degraded
  admission. There is no shadow-allow path and no degraded mode.
- A policy change needs no gear-side reconfiguration and no cache invalidation.
  This covers new tenants, new calling gears, and new GTS type grants.
- The ingestion path waits for a PDP decision before it accepts an entry, inside
  the budget of `cpt-cf-usage-collector-nfr-ingestion-latency` (p95 ≤ 200 ms).
  How many PDP calls one request makes is a DESIGN concern, not a decision here.
- The PDP drives all read scoping. A caller-supplied filter can only narrow the
  authorized scope, never widen it.
- Uniformity becomes a review obligation instead of a structural property. The
  shared helper must remain one definition, and every component must use it. The
  alternative — a centralized adapter — makes the guarantee structural instead.
- The gateway owns authentication, so the gear's readiness model carries no fact
  about an authentication client. The gear probes only PDP-client readiness at
  startup. This narrows the gear's failure surface and moves the whole
  authentication failure mode to the gateway.

### Confirmation

- Design review against the DESIGN component model, to show that every component
  that owns a guarded operation runs the shared check before plugin dispatch.
- Authorization conformance tests, which cover permit, deny, and
  constraint-scoped outcomes on every operation.
- Negative tests, which show that PDP unavailability produces deterministic
  failure rather than fallback admission.

## Pros and Cons of the Options

### PDP-centric authorization

`authz-resolver` makes every check against the live policy graph. The gear holds
no access state of its own.

- Good, because policy lives in one place and reaches every metering operation
  through one path.
- Good, because it removes every fallback path that can mask a denied operation,
  which is what fail-closed requires.
- Good, because it avoids the cache-coherence work that a local ACL table
  creates.
- Neutral, because every authorized operation costs a PDP call. The cost must be
  measured against the ingestion latency budget.
- Bad, because PDP unavailability is a hard dependency on the ingestion path.
  The platform owns this risk, not the gear.

### In-collector ACL cache

The gear keeps a per-tenant and per-GTS-type access table, refreshed from a
platform source of truth, and decides locally on each request.

- Good, because ingestion does not block on PDP availability, and the per-entry
  authorization cost stays in-process.
- Bad, because it duplicates platform authorization state and re-creates the
  drift surface that the centralized PDP removes.
- Bad, because refresh policy, invalidation, and audit become gear
  responsibilities, and the gear does not own policy.
- Bad, because a stale table can produce silent over-permissive permits, which
  breaks the fail-closed posture.

### Hybrid model

A short-lived cache of PDP results covers the high-rate ingestion path.
Low-frequency operations still call the PDP directly.

- Good, because the PDP keeps authority over low-frequency operations, and PDP
  load for ingestion drops.
- Bad, because any positive-cache TTL re-creates the drift surface that the
  centralized PDP removes.
- Bad, because cache coherence and revocation become gear responsibilities,
  against `cpt-cf-usage-collector-constraint-no-business-logic`.
- Neutral, because platform-side PDP scaling reaches the same lower PDP load
  without a gear-side cache.

## More Information

DESIGN specifies the enforcement mechanics that this decision implies: the
shared helper, its placement relative to the framework layer, and the PDP
contract surface. They live in the «PDP Authorization Posture» subsection of the
DESIGN component model and in the
`cpt-cf-usage-collector-contract-authz-resolver` entry. This ADR does not
restate them.

Related decisions:

- `cpt-cf-usage-collector-adr-caller-supplied-attribution` — the attribution
  tuple that this check authorizes.
- `cpt-cf-usage-collector-adr-registry-owned-typing` — why GTS type declarations
  are platform-global.
- `cpt-cf-usage-collector-adr-append-only-invalidation` — why a correction is an
  ordinary ingestion.
- `cpt-cf-usage-collector-adr-mandatory-idempotency` — what keeps a retry after
  a PDP failure safe.

The PDP and authentication contracts are platform-level. This decision does not
redefine them.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

- `cpt-cf-usage-collector-fr-ingestion-authorization` — every entry is
  authorized against its full attribution tuple before plugin dispatch.
- `cpt-cf-usage-collector-fr-tenant-isolation` — the PDP-returned scope, and the
  gear's check of each attribution against it, are the only mechanism for tenant
  isolation.
- `cpt-cf-usage-collector-principle-pdp-centric-authorization` — the design
  principle that this decision codifies.
- `cpt-cf-usage-collector-principle-fail-closed` — applies the fail-closed
  posture to the authorization path.
- `cpt-cf-usage-collector-contract-authz-resolver` — the PDP contract that every
  check uses.
- `cpt-cf-usage-collector-component-ingestion-gateway` — guards every accepted
  entry, measurement and invalidation alike, against the caller's attribution.
- `cpt-cf-usage-collector-component-query-gateway` — guards aggregation, raw
  query, and point-lookup reads against the caller's authorized scope.
- `cpt-cf-usage-collector-component-feed-gateway` — guards feed subscription and
  page reads against the caller's authorized scope.
