---
status: accepted
date: 2026-09-10
decision-makers: BSS Orders team (Architecture)
---

# ADR-0003: Read Order State Only Through One Seam, Never From Process State


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Convention documented per slice](#convention-documented-per-slice)
  - [A shared read-through port (chosen)](#a-shared-read-through-port-chosen)
  - [Local caching reconciled asynchronously](#local-caching-reconciled-asynchronously)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-workflow-adr-process-state-non-authoritative`
## Context and Problem Statement

PRD §6.1 fixes two authorities: Orders Lifecycle is authoritative for the commercial order document and order state; this gear's process audit and saga log are authoritative only for process-execution progress, independently of durable-execution engine history (ADR-0001). Every slice — trigger binding, approval execution, fulfillment planning, provisioning, saga/compensation, hold/cancel, read/authz — is tempted to cache or infer order state locally to avoid a round trip to Lifecycle. If that temptation is resolved per handler, the two Orders Lifecycle seam rules this gear depends on — R1 (out-of-order triggers resolved by reading current order state and version from Lifecycle before acting) and R5 (process state never presented as authoritative commercial order state) — become a discipline argued nine times rather than a property. How should the design make process state non-authoritative structurally, rather than by convention repeated in every slice?

## Decision Drivers

* R1 and R5 are both `p1` and both depend on the same fact: no slice may treat its own cached or derived view of order state as ground truth.
* A stale or superseded-version trigger acted on without re-reading Lifecycle causes exactly the double-provisioning and conflicting-instance risk PRD §6.1's start contract calls out.
* Process state (step progress, saga log, retry counters) has its own zero-loss durability requirement (ADR-0001) and must remain useful for audit and replay even though it is not authoritative for commercial semantics — the design must not conflate "not authoritative" with "discardable."
* Nine slices (ADR-0002) each have a plausible reason to read order state; a rule stated in the PRD text but enforced ad hoc per slice will drift as slices are built independently and on different schedules.
* The sibling `orders-lifecycle` gear enforces its own single-writer analogue (ADR-0001 there) structurally through one engine; the equivalent problem here is single-reader-of-truth, not single-writer, and needs its own structural mechanism.

## Considered Options

* Convention: document R1 and R5 in each slice's design section and rely on code review to catch violations
* A shared read-through port: every slice reads current order state and version exclusively through one gear-owned interface that always calls Orders Lifecycle (or a short-lived cache it owns and invalidates), and no slice is permitted its own independent path to order state
* Allow slices to cache order state locally for performance, reconciled asynchronously against Lifecycle

## Decision Outcome

Chosen option: **a shared read-through port**, because it is the only option under which R1 and R5 are unbreakable by a slice added later rather than re-argued in each of the nine slice reviews. Every slice that needs current order state or version calls the same gear-owned port; that port's only source of truth is Orders Lifecycle, and it never serves a value derived from this gear's own process-audit or saga-log tables. Process-execution state remains fully durable and queryable for audit, replay and operator visibility (per ADR-0001), but no code path exposes it to a caller as if it were order state, and no code path treats a locally held order-state value as current without going back through the port when a trigger requires re-evaluation (R1).

### Consequences

* Every slice's guard evaluation for "is this trigger still applicable" routes through the shared port rather than through a locally read field, making a superseded-version trigger structurally unable to act on a stale order view.
* The port becomes the one place a Lifecycle read failure, timeout, or degraded-dependency behavior (`fr-owf-dependency-resilience`) is handled, rather than nine independent retry/backoff implementations.
* Operator-facing and audit-facing surfaces (progress reads, manual-task views) must label process-execution fields as process state explicitly, and must never render them as if they were the order's commercial status, since the port and the audit log are deliberately different objects with different authorities.
* A slice may still hold a short-lived, request-scoped copy of an order-state read obtained through the port, but may not persist it as an independent order-state cache that outlives the request that read it; if a longer-lived cache is later needed for latency, it must be owned by the port, not by an individual slice.
* This decision does not relax the zero-loss durability requirement on process-execution state (ADR-0001); it only forbids that state from being presented or consulted as commercial order truth.

### Confirmation

Confirmed by a structural test (or lint rule) asserting no slice module holds a direct dependency on Lifecycle's order-state read path other than through the shared port, and a scenario test replaying an out-of-order trigger for a superseded `orderVersion` and asserting it is ignored because the port's live read, not a cached value, determines applicability (R1). A second check asserts every operator-facing or audit surface that renders process-execution fields carries a label distinguishing them from order state (R5).

## Pros and Cons of the Options

### Convention documented per slice

* Good, because it requires no new shared component.
* Good, because each slice author can start immediately without waiting on a shared port's design.
* Bad, because R1 and R5 must be re-verified in nine separate reviews, and a violation in one slice does not fail any other slice's tests.
* Bad, because nothing prevents a later slice from reading its own cached field instead of calling Lifecycle, and that regression would be silent until a stale-trigger incident surfaced it.

### A shared read-through port (chosen)

* Good, because R1 and R5 become properties of one code path, matching how ADR-0001 in the sibling `orders-lifecycle` gear made its own guarantees structural rather than conventional.
* Good, because dependency-resilience handling for Lifecycle reads is implemented once and reused by all nine slices.
* Neutral, because it introduces a shared component that every slice depends on, mirroring the same chokepoint trade-off ADR-0001 in this gear already accepts for the process engine.
* Bad, because a slice cannot optimize its own read pattern independently; any latency tuning must happen inside the shared port.

### Local caching reconciled asynchronously

* Good, because it could reduce read latency for high-frequency slices.
* Bad, because an asynchronous reconciliation window reintroduces exactly the dual-view risk §6.1 exists to prevent: a slice could act on a locally cached order state that has already been superseded in Lifecycle.
* Bad, because "eventually consistent with Lifecycle" is incompatible with R1's requirement to resolve out-of-order triggers by reading current state before acting.

## More Information

This ADR complements ADR-0001 (durable-execution substrate) rather than restating it: ADR-0001 establishes that this gear's own process-execution record is the audit SoR for process progress; this ADR establishes that the same process-execution record is never a substitute for reading commercial order state, and that both facts are enforced through a single shared read path rather than through per-slice discipline.

## Traceability

- **PRD**: [PRD.md](../PRD.md) — §6.1
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-workflow-fr-owf-process-state-nonauth` — this decision is the structural mechanism (the shared port) by which process state is never presented as authoritative commercial order state
* `cpt-cf-bss-orders-workflow-fr-owf-start-contract` — out-of-order and duplicate-trigger resolution (R1) reads current order state and version through the same shared port before acting
* `cpt-cf-bss-orders-workflow-fr-owf-terminal-order-events` — termination on a terminal Lifecycle event depends on the port's live read to detect the terminal state and the superseded-version case
* `cpt-cf-bss-orders-workflow-fr-owf-dependency-resilience` — Lifecycle read-failure handling is centralized in the shared port rather than duplicated per slice
