---
status: accepted
date: 2026-09-10
decision-makers: BSS Orders team (Architecture)
---
# ADR-0008: Process Events Are Published Via A Transactional Outbox, Disjoint From Lifecycle's State Events


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Transactional outbox with an asynchronous drain (chosen)](#transactional-outbox-with-an-asynchronous-drain-chosen)
  - [Synchronous publish inside the transition](#synchronous-publish-inside-the-transition)
  - [Publish after commit, in the same request, with a reconciliation sweep](#publish-after-commit-in-the-same-request-with-a-reconciliation-sweep)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-workflow-adr-outbox-process-events`
## Context and Problem Statement

Orders Workflow must publish six named process events (`OrderFulfillmentStarted`,
`OrderFulfillmentStepCompleted`, `OrderFulfillmentCompleted`, `OrderFulfillmentAborted`,
`OrderApprovalRequested`, `OrderApprovalEscalated`) with idempotent, at-least-once consumer
semantics, and must not publish order-**state** events — that set is owned and enumerated
exclusively by Orders Lifecycle. How should Workflow publish its six events reliably, without
duplicating or colliding with Lifecycle's state-event contract, and what does that publication
mechanism cost against the PRD's delivery target?

## Decision Drivers

* "Zero silent drops" for operationally significant process transitions — an operator dashboard that misses `OrderFulfillmentAborted` cannot act on it.
* The process-event set must remain strictly disjoint from Lifecycle's state-event set to keep consumers' event models clean and prevent dual publication of the same semantic change.
* The PRD sets a p95 < 30 s delivery target for the six process events (`cpt-cf-bss-orders-workflow-nfr-owf-event-latency`), which any publication mechanism must be measured against honestly.
* Orders Lifecycle's own outbox ADR (`cpt-cf-bss-orders-lifecycle-adr-outbox-publication`) already establishes the shared-baseline shape for transactional, per-order-ordered event publication in this system; deviating from it needs its own rationale.
* Fulfillment step and process transitions and their event declarations happen inside the same durable-execution transaction boundary as the state change being reported.

## Considered Options

* **Transactional outbox with an asynchronous drain** — enqueue a row in the same transaction as the process-state change; a separate worker publishes it
* **Synchronous publish inside the transition**
* **Publish after commit, in the same request, with a reconciliation sweep**

## Decision Outcome

Chosen option: "Transactional outbox with an asynchronous drain", because it is the only option
that makes an event-declaring committed transition and its outbox row atomic, matching the
shared-baseline shape Lifecycle's own outbox ADR already established for this system, without
placing a network call inside a durable-execution transition. Exactly one outbox row is enqueued
per event-declaring committed transition; a separate drain publishes it afterward with per-order
ordering preserved.

**The enqueue is sequenced with the local state mutation, never after a cross-gear call.** An
event's outbox row is written in the same transaction as the *local* process-state change that
makes the event true, and that transaction is committed before any synchronous call to another
gear. `OrderFulfillmentAborted` is the case that forces the rule to be stated: it reports an
order-level abort that is also reflected onto Orders Lifecycle, and Lifecycle's acknowledgement is
a network call that cannot share a transaction with this gear's own write. Enqueuing the event
"after either report" would therefore leave a window in which the acknowledged Lifecycle call has
happened and the outbox row has not, and a crash inside it drops the event permanently while the
"exactly one row per event-declaring transition" invariant still reads as satisfied — because no
transition ever declared it. The correct order is: commit the local abort transition **and** its
outbox row together, then make the Lifecycle call as a separately retried, idempotent step keyed
under the Lifecycle transition-call idempotency family (ADR-0006). The event describes what this
gear decided, which is a local fact; it does not describe what Lifecycle acknowledged. The same
rule binds every one of the six events without exception.

The process-event set is deliberately kept **non-overlapping** with Lifecycle's order-state event
set: Workflow **MUST NOT** publish order-state events at all — that enumeration is Lifecycle's
alone, stated once to prevent drift. The naming split between the two gears is deliberate rather
than accidental: Lifecycle's state event on the `fulfillment_failed` transition is
`OrderFulfillmentFailed`; Workflow's corresponding **process** event, published when order-level
fulfillment fails or the workflow is cancelled, is `OrderFulfillmentAborted`. The two names refer to
related-but-distinct facts owned by different systems of record, and are not interchangeable.

### Consequences

* Workflow must maintain its own outbox table and drain, structurally parallel to but operationally independent of Lifecycle's outbox — the two gears do not share an outbox or a sequence space.
* Every one of the six process events must be declared at the same transition that also mutates **local** process state, inside one transaction, so the "declared implies enqueued" invariant holds without exception. Where the same fact is also reported across a gear boundary, the boundary call follows the committed transaction and never precedes the enqueue.
* Consumers of Workflow's process events must de-duplicate via event ID under at-least-once delivery, exactly as Lifecycle's consumers already must.
* **The outbox-vs-p95<30s tension is real and must be disclosed rather than asserted away.** Asynchronous drain publication is not on the commit path, and its delivery latency is bounded by drain scheduling, batching and downstream broker behavior rather than by the transition itself; whether the chosen batching and lease shape can hold p95 < 30 s under the drain's own load is a Design-level measurement, not a guarantee this ADR can make. If the drain cannot hold the budget under production load, the resolution is a drain-tuning or budget-revision decision routed back to Product, not a silent widening of the number reported against the NFR.
* An undeliverable process event, after a bounded delivery-count cap, is parked **on its own outbox row** by setting `owf_event_outbox.dead_lettered_at`, and raises a delivery-poisoning alert to the fulfillment-operator queue. It does **not** become an `owf_dead_letter_record`. ADR-0009 scopes that store exclusively to *inbound* trigger and callback delivery, and `owf_dead_letter_record.source` — "the inbound trigger or callback that failed" — has no legal value for an outbound publication, so routing an undeliverable event there would either invent a source value or leave the column meaningless. The two stores answer two different questions ("a delivery into this gear kept failing" vs. "a publication out of this gear kept failing") and are not merged. Both surface to the same operator queue as delivery-poisoning alerts rather than as remediation tasks, so the operational path is shared even though the records are not.

### Confirmation

Verified by a cross-table invariant test asserting exactly one outbox row per event-declaring
committed process transition; by a test asserting no code path in Workflow ever writes a row to
Lifecycle's state-event table or vice versa; by a test asserting the `OrderFulfillmentAborted` outbox row is present after a crash injected between the committed local abort transition and the Lifecycle acknowledgement, and that no code path enqueues an outbox row after a cross-gear call returns; by a schema check asserting no code path writes an `owf_dead_letter_record` row from the outbox drain; by a naming-registry check asserting
`OrderFulfillmentAborted` and `OrderFulfillmentFailed` are never used interchangeably in payload or
audit text; and by a drain-lag metric measured independently against the 30 s delivery budget so
the tension recorded above is an observed number, not an assumption.

## Pros and Cons of the Options

### Transactional outbox with an asynchronous drain (chosen)

* Good, because the event-declaring transition and its outbox row are atomic — an event can never be lost for a committed transition nor emitted for one that rolled back.
* Good, because no network call sits inside the durable-execution transition boundary.
* Good, because it matches the shared-baseline shape Lifecycle already uses, reducing cross-gear cognitive load for operators and auditors.
* Bad, because publication is asynchronous by construction, which is the direct source of the p95 < 30 s tension recorded above.
* Bad, because it requires Workflow to operate and monitor its own drain infrastructure.

### Synchronous publish inside the transition

* Good, because it minimizes the gap between state change and event visibility, most directly supporting the p95 < 30 s target.
* Bad, because it puts broker latency inside the transition, and a broker outage becomes a process-execution outage.
* Bad, because it is not atomic across two systems: a failure between commit and publish either drops the event or reports a transition nobody committed.

### Publish after commit, in the same request, with a reconciliation sweep

* Good, because it releases the transition before the network call, avoiding synchronous publish's lock-holding cost.
* Bad, because the atomicity gap remains, only smaller, and still requires a reconciliation sweep — which is an outbox with extra steps and less rigor.

## More Information

This ADR intentionally does not repeat Lifecycle's eleven-event enumeration; see
`cpt-cf-bss-orders-lifecycle-adr-outbox-publication` for the sibling gear's outbox shape, which this
decision follows for consistency across the two gears' event infrastructures.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-workflow-fr-owf-process-events` — fixes the outbox publication mechanism for all six named process events and the non-overlap rule with Lifecycle's state-event set, including the `OrderFulfillmentAborted`/`OrderFulfillmentFailed` naming split
* `cpt-cf-bss-orders-workflow-nfr-owf-event-latency` — records the honest tension between asynchronous outbox delivery and the p95 < 30 s target, and fixes the drain-lag metric as the instrument that measures it
* `cpt-cf-bss-orders-workflow-nfr-owf-audit` — the outbox invariant is the "zero silent drops" mechanism for process events, mirrored from Lifecycle's audit-completeness guarantee
* `cpt-cf-bss-orders-workflow-component-foundation` (**event outbox**) — sole writer of `owf_event_outbox` and owner of the drain; the enqueue is this component's, not fulfillment orchestration's
* `cpt-cf-bss-orders-workflow-component-foundation` (**step executor**) — declares the event alongside the local state mutation it commits, which is where the atomicity rule above binds
* `cpt-cf-bss-orders-workflow-component-approval-execution` (**approval gate manager**) and `cpt-cf-bss-orders-workflow-component-approval-execution` (**escalation timer owner**) — declare `OrderApprovalRequested` and `OrderApprovalEscalated` respectively; two of the six events are approval-execution's, so no single fulfillment component can own the event set
* `cpt-cf-bss-orders-workflow-component-manual-tasks` (**operator task queue**) — receives the delivery-poisoning alert when an outbox row exhausts its delivery cap
