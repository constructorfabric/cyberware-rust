---
status: accepted
date: 2026-09-08
decision-makers: BSS Orders team
---

# ADR-0002: Decompose The Design As A Foundation Plus Seven Capability Slices


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [One transition engine plus seven capability handlers (chosen)](#one-transition-engine-plus-seven-capability-handlers-chosen)
  - [A capability per service, each owning its own writes](#a-capability-per-service-each-owning-its-own-writes)
  - [One monolithic order service with no internal boundary](#one-monolithic-order-service-with-no-internal-boundary)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-lifecycle-adr-slice-decomposition`
## Context and Problem Statement

The order document has eleven states, twenty-five transitions, twenty-five endpoints and two
callers, spanning six PRD capability areas. Every capability — draft authoring, the submit gate,
amendment, preconditions, the workflow seam, hold and expiry, read and authorization — needs the
same four guarantees over the same aggregate: authorization, idempotency, an audited transition and
an optimistic version check.

**How should the runtime be decomposed?** Concretely: does each capability own its own write path
to the order, or is there one component that owns every state change with the capabilities as
handlers around it? This determines what a capability *is* at runtime — a service with its own
persistence, or a guard set plus a contribution to somebody else's transaction — and it determines
whether the four guarantees are properties of one code path or a discipline repeated eight times.

The document layout follows from that answer rather than driving it: a runtime boundary is what
makes a review boundary meaningful. That layout is recorded as `DECISIONS.md` D-02 and D-03, not
here.

## Decision Drivers

* The correctness core (transition contract, idempotency, state machine, schema) must be reviewable and testable independently of commercial policy.
* Implementation needs a phase order with an explicit dependency graph, because the capture-to-`submitted` path is shippable long before the workflow seam.
* Two capability areas depend on unagreed upstream asks; their design must be isolatable so an upstream change is a boundary change.
* The four BSS sibling gears — `pricing`, `rating`, `subscriptions` and `ledger` — all use an index-plus-slices shape, and divergence costs review effort.
* A single document carrying every design item would be ~1 500 lines of interleaved normative content with no natural review boundary.

## Considered Options

* **One transition engine plus seven capability handlers** — the engine owns every write to the order; a handler declares guards and supplies contributions, and owns no write path
* **A capability per service, each owning its own writes** to the order aggregate, coordinating through the database
* **One monolithic order service** with no internal capability boundary — every operation a method on one component

## Decision Outcome

Chosen option: **one transition engine plus seven capability handlers**. The engine
(`01-foundation`) owns the aggregate, the version chain, the transition table, the idempotency
registry, the audit trail and the outbox — and is the only component with a write grant on any of
them. A handler (`02`–`08`) owns its predicate set, its reason names, its own tables where it
introduces any, and the composition of its contribution; it owns no write to the order and reaches
it only through the engine's transition operation.

**This is the decomposition, and the seven-slice document layout is its shadow.** Each handler is a
document because each is a runtime boundary with its own guards, reasons and upstream dependencies;
`ADR/0001` is why the engine is one component rather than a library, and this ADR is why there are
seven handlers around it rather than one component or eight independent services.

The per-service option was rejected on a single ground: the boundary between
makes the reader check two places for every question.

### Consequences

* The correctness core is reviewable in isolation, and its own review found defects that a 1 500-line combined document would very likely have buried.
* Each capability's guards, reasons and sequences live with the capability, so a slice author touches one file and a reviewer reads one file.
* The dependency table in `design/README.md` becomes the build-order authority and must be kept truthful; the 2026-09-08 review found six missing edges, which is the recurring cost of this shape.
* Facts derived across the set — table inventories, endpoint inventories, event counts, worker counts — must be reconciled deliberately after any change, because no single document owns them. The same review found seven count inconsistencies of exactly this kind.
* Template sections that belong to the gear rather than a capability (`§3.4`, `§3.5`, `§3.8`) are thin or inherited in most slices. This is accepted as the price of uniform structure rather than padded.
* **A handler cannot be deployed independently of the engine.** It has no write path of its own, so the decomposition is a boundary inside one deployable, not a service split. Anything wanting independent deployment would need its own aggregate, which is `ADR/0001`'s decision to reverse rather than this one's.
* **Seven handlers means seven guard sets registered against one table**, so the engine's startup registration is the integration point where a handler's mistake surfaces — a guard declared against a row that does not exist fails the boot rather than a request.
* The layout consequence is **ten design documents** — `DESIGN.md`, `design/README.md` and the eight slices — inside a nineteen-artifact gear set that also carries seven ADRs, the decisions register and the upstream-requirements register. That matches the four sibling gears, so reviewers and tooling encounter a familiar shape; the recurring cost of the shape is derived facts drifting between documents, which is why the set is gated by a machine-checked invariant suite.

### Confirmation

Verified by the engine holding the only write grant on the tables of `01 §3.7` — no handler appears
as a writer anywhere in the set — by every handler's algorithm reaching state through the engine's
transition operation rather than a direct write, which the invariant suite asserts as its
no-pre-engine-refusal and single-writer checks, and by startup failing where a handler registers a
guard against a transition row that does not exist. Drift in the decomposition is therefore a boot
failure or a failing check, not a review observation.

## Pros and Cons of the Options

### One transition engine plus seven capability handlers (chosen)

* Good, because the four `p1` guarantees are properties of one code path, so a handler cannot forget one — which is the whole reason `ADR/0001` picks an engine, and this ADR is what keeps the handlers around it from acquiring write paths.
* Good, because a handler's blast radius is its own predicate set and reasons; an upstream change is a boundary change rather than an engine change, which matters for the two capabilities that depend on unagreed asks.
* Good, because startup registration is a real integration point: a guard declared against a non-existent transition row fails the boot rather than a request.
* Good, because it matches `pricing`, `rating`, `subscriptions` and `ledger`, so reviewers and tooling encounter a familiar shape.
* Bad, because a handler cannot be deployed or scaled independently — it has no write path, so this is a boundary inside one deployable, not a service split.
* Bad, because seven handlers contributing to one transaction means the engine's contribution contract is wide, and every new handler widens it.
* Bad, because the boundary produces ten design documents whose derived facts drift, which is why the set needs a machine-checked invariant suite to stay coherent.

### A capability per service, each owning its own writes

* Good, because each capability scales and deploys independently.
* Good, because a capability team owns its own persistence and release cadence.
* Bad, because the four guarantees become eight implementations, which is the defect `ADR/0001` exists to remove — authorization, idempotency, audit and the version check would each be repeated.
* Bad, because the aggregate would have eight writers, so the single-writer constraint and its append-only grants could not hold.
* Bad, because a transition spanning two capabilities would need a distributed transaction or a saga, reintroducing the partial-commit reconciliation the design has none of.

### One monolithic order service with no internal boundary

* Good, because there is no contribution contract to maintain and no registration step.
* Good, because it is the least code for the first capability shipped.
* Bad, because commercial policy and the correctness core interleave, so a gate change and an idempotency change are reviewed as one thing.
* Bad, because there is no unit at which an upstream dependency is isolated, so the unagreed asks would touch the whole component.
* Bad, because the guard set becomes a growing conditional inside the transition rather than data registered against a row.

## More Information

The location convention was decided separately: slices live in `docs/design/NN-*.md` following
the BSS sibling gears, rather than in `docs/features/` where the platform's registered FEATURE
artifacts live. That choice trades away registry visibility — `cfs where-used`, per-artifact
validation and `cfs spec-coverage` marker tracing do not reach these paths — and is recorded as
D-02 in [`../DECISIONS.md`](../DECISIONS.md).

## Traceability

- **Governing requirements**: `cpt-cf-bss-orders-lifecycle-fr-order-state-machine`, `cpt-cf-bss-orders-lifecycle-fr-order-create`, `cpt-cf-bss-orders-lifecycle-fr-order-submit`

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-lifecycle-fr-order-submit` — the gate is a handler with its own predicate set and outbound ports, so its unagreed upstream dependencies are a boundary change rather than an engine change
* `cpt-cf-bss-orders-lifecycle-fr-order-amendment` — versioning is a handler, so the carry-forward and re-pin rules sit beside the version chain they act on without owning its writes
* `cpt-cf-bss-orders-lifecycle-fr-order-atomic-fulfillment` — the workflow seam is a handler, which is what lets the sibling gear's contract change without touching the engine
* `cpt-cf-bss-orders-lifecycle-component-transition-engine` — seven handlers register their guards against this component at startup, making registration the integration point where a handler's error becomes a boot failure
- **PRD**: [`../PRD.md`](../PRD.md) — §6 functional requirements, §12 acceptance criteria
- **DESIGN**: [`../DESIGN.md`](../DESIGN.md) §1.3, §3.2; [`../design/README.md`](../design/README.md)
- **Decisions register**: [`../DECISIONS.md`](../DECISIONS.md) — D-02, D-03
