---
status: proposed
date: 2026-04-20
decision_owner: Platform Engineering
approver: Architecture Review Board
scope: licensing-service; all ModKit modules integrating licensing and quota enforcement
---

# ADR-0002: Unified Licensing and Quota Module

**ID**: `cpt-cf-licensing-service-adr-unified-licensing-quota-module`

## Context and Problem Statement

The platform previously specified two separate modules: a `licensing-service` owning business entitlement truth and a
standalone `quota-service` owning runtime reservation, usage accounting, and reconciliation. Review of both PRDs
exposed a tightly coupled decision path: every quota decision starts with an entitlement check and an effective-limit
lookup, and every effective-limit lookup eventually participates in a quota-admission decision.

Two independent modules forced consumers to either orchestrate two synchronous calls for one logical decision, or
forced one service to synchronously call the other on every hot-path request. Both shapes carried latency,
availability, and ownership-boundary risks without adding meaningful separation of concerns in a modular monolith.

This ADR records the decision to unify both capabilities into a single ModKit module, `licensing-service`, with
internal layering that preserves ownership separation without the coordination overhead of a service boundary.

## Decision Drivers

- A single consumer-facing decision covering entitlement truth and runtime admission with one canonical contract
- One SDK surface instead of two for all ModKit and CyberFabric consumers
- Low-latency hot-path decisioning without a mandatory synchronous inter-service hop
- Preservation of clear ownership boundaries between commercial licensing truth and runtime quota state
- Preservation of federated authority routing, legacy VendorA adaptation, and shadow migration already established in
  ADR-0001
- Avoidance of duplicate policy, idempotency, observability, and reconciliation plumbing across two modules
- Preserving a credible path to split the module later if maintainability, latency, or blast-radius metrics require it

## Considered Options

- **Option A**: Keep licensing and quota as two separate modules, each with its own API and SDK, and require consumer
  orchestration across both
- **Option B**: Keep two separate modules but make one the mandatory orchestrator of the other
- **Option C**: Unify licensing and quota into one ModKit module with explicit internal layering (entitlement layer
  and quota layer)

## Decision Outcome

Chosen option: **Option C — Unified licensing and quota module with internal layering**.

The platform will expose one ModKit module, `licensing-service`, that owns both canonical licensing APIs and runtime
quota decisioning, reservation lifecycle, usage accounting, and reconciliation. Internally, the module maintains two
layers:

- **Entitlement layer**: Authority routing, federation, legacy adaptation, shadow comparison, canonical entitlement
  and effective-limit read models, and commercial licensing truth.
- **Quota layer**: Policy resolution, runtime admission, reservation lifecycle, usage accounting, and reconciliation.

The quota layer **MUST NOT** independently redefine commercial licensing truth for a scope that is already
authoritative in the entitlement layer. The layer ownership matrix is specified in
`cpt-cf-licensing-service-fr-internal-ownership`.

The former standalone `quota-service` module is removed from the repository. Its requirements are incorporated into
the Licensing Service PRD §5.5–§5.11.

ADR-0001 (federated licensing authority) remains in force and is not superseded; it covers the authority-routing
decision that lives inside the entitlement layer of this unified module.

### Consequences

- Good, because consumers receive one explainable decision covering both entitlement and quota state through one SDK
- Good, because hot-path decisions no longer require a synchronous inter-service hop between licensing and quota
- Good, because policy lifecycle, idempotency, observability, and reconciliation plumbing exist only once
- Good, because `correlation_id` and `client_operation_id` propagate through one module rather than across a network
  boundary
- Good, because federated authority routing, legacy VendorA adaptation, and shadow migration are preserved unchanged
  by ADR-0001
- Bad, because the unified module is larger and has a broader blast radius than either of the previous two modules
- Bad, because internal layer discipline must be enforced by code review, lints, and internal API boundaries to
  prevent the quota layer from silently claiming entitlement truth
- Bad, because a future split back into two modules is non-trivial if this decision is reversed

### Confirmation

- Code review: verify the `entitlement` and `quota` internal layers communicate through a stable internal trait
  boundary and that the quota layer does not construct or mutate entitlement-owned state
- Contract tests: verify admission decisions carry both the canonical `decision_outcome` and `policy_effect` pair and
  the quota-layer `admission_outcome` with the deterministic mapping specified in
  `cpt-cf-licensing-service-fr-soft-hard-outcomes`
- Integration tests: verify the whole path (entitlement → effective-limit → quota admission → reservation → commit)
  preserves `correlation_id` and `client_operation_id` end to end
- Ownership audit: verify that no quota-layer code owns `entitlement_existence`, `feature_enablement`,
  `effective_licensed_limit`, or `commercially_authoritative_consumed_quantity`
- Plugin isolation: verify the legacy VendorA adapter remains a ModKit plugin behind the Licensing Service public API
  and is not consumed directly by platform modules

## Pros and Cons of the Options

### Option A — Two separate modules with consumer orchestration

Each consumer orchestrates a licensing decision and a quota admission call for one logical operation.

- Good, because each module is smaller and independently evolvable
- Good, because blast radius per module is narrower
- Bad, because consumers reimplement orchestration and idempotency correlation across two boundaries
- Bad, because two synchronous calls on the hot path inflate latency and reduce composed availability
- Bad, because `correlation_id` and `client_operation_id` must cross a network boundary for every decision
- Bad, because policy, observability, and reconciliation infrastructure is duplicated across two modules

### Option B — Two modules with one mandatory orchestrator

One module (typically licensing) becomes the only permitted caller of the other, hiding orchestration from consumers.

- Good, because consumers see only one API
- Bad, because one module becomes a privileged client of the other and must embed the other module's semantics
- Bad, because internal orchestration rules are carried over a network boundary for no separation-of-concerns benefit
- Bad, because failure modes, retries, and degraded-mode policy must be reasoned about in two services for one
  logical decision

### Option C — Unified module with internal layering

One ModKit module exposes both capabilities with internal entitlement and quota layers communicating through a
stable internal trait boundary.

- Good, because consumers receive one decision, one SDK, and one explainable response
- Good, because hot-path decisions stay in-process with no inter-service hop
- Good, because policy, idempotency, observability, and reconciliation exist once
- Good, because internal layering preserves ownership separation without network coordination
- Bad, because the module is larger and requires layering discipline enforced by code review and lints
- Bad, because splitting back into two modules later is non-trivial if growth or latency metrics require it

## Review Cadence

Revisit this decision when any of the following becomes true:

- Module size, compile time, or release blast radius materially degrades maintainability
- Hot-path latency budgets cannot be met without separating runtime admission from entitlement resolution
- Organizational ownership splits such that entitlement and quota capabilities are owned by separate teams with
  independent release cadences
- A future deployment requires running runtime admission in a different process or region from entitlement authority

## Traceability

- **PRD**: [../PRD.md](../PRD.md) — §4.3 Unified Module Rationale and §5.5 Internal Ownership and Decision
  Orchestration
- **Companion ADR**: [0001-cpt-cf-licensing-service-adr-federated-licensing-authority.md](./0001-cpt-cf-licensing-service-adr-federated-licensing-authority.md)
- **Superseded Module**: former standalone `quota-service` module; removed from the repository, requirements
  incorporated into §5.5–§5.11 of Licensing Service PRD

This decision directly addresses the following planned requirements or design elements:

- `cpt-cf-licensing-service-fr-internal-ownership`
- `cpt-cf-licensing-service-fr-decision-orchestration`
- `cpt-cf-licensing-service-fr-soft-hard-outcomes`
- `cpt-cf-licensing-service-fr-retry-after-semantics`
- `cpt-cf-licensing-service-fr-deferred-admission`
- `cpt-cf-licensing-service-fr-preview-vs-reserve`
- `cpt-cf-licensing-service-fr-persisted-grant-update`
- `cpt-cf-licensing-service-fr-idempotent-lifecycle`
- `cpt-cf-licensing-service-fr-reserved-consumed-independence`
- `cpt-cf-licensing-service-fr-shared-recovery-state-machine`
- `cpt-cf-licensing-service-fr-idempotency-vs-archive`
- `cpt-cf-licensing-service-fr-absolute-stale-write-protection`
