---
status: accepted
date: 2026-09-10
decision-makers: BSS Orders team (Architecture)
---
# ADR-0009: Manual Tasks And Dead-Letter Records Are Distinct First-Class Objects


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Two distinct first-class objects with distinct entry conditions (chosen)](#two-distinct-first-class-objects-with-distinct-entry-conditions-chosen)
  - [A single unified "failure record" object with a type discriminator](#a-single-unified-failure-record-object-with-a-type-discriminator)
  - [Route exhausted fulfillment-step remediation into dead-letter as well](#route-exhausted-fulfillment-step-remediation-into-dead-letter-as-well)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-workflow-adr-manual-task-dead-letter-separation`
## Context and Problem Statement

Orders Workflow has two different kinds of "something needs a human, or at least an inspectable
record": a fulfillment step that exhausts remediation, and an inbound trigger or callback
(Lifecycle, Subscriptions, Payments, Generic Approval) that keeps failing delivery. Both look like
"parking a failure somewhere inspectable," which invites collapsing them into one object. Should a
fulfillment step's exhausted-remediation object and a poisoned-delivery dead-letter record be the
same object, or two distinct objects with distinct entry conditions?

## Decision Drivers

* PRD §6.4's manual-task / fail-fast policy already gives a permanently failed fulfillment step an inspectable object — the manual task (remediation policy) or the tracked incident (fail-fast) — before dead-letter enters the picture at all.
* 100% of permanently failed order lines must produce a tracked manual task with zero silent failures, per `cpt-cf-bss-orders-workflow-nfr-owf-manual-task-sla`; that guarantee must not be diluted by a second, competing object for the same failure.
* Inbound triggers and callbacks fail for a structurally different reason: delivery exhaustion against a finite retry cap, not fulfillment-step remediation exhaustion.
* A dead-letter record must never be mistaken for, or promoted into, an order state — Lifecycle's state machine is not something Workflow may extend by accretion.
* Operators need to be able to tell, from the object type alone, whether they are looking at "a fulfillment step needs manual remediation" or "a delivery kept failing and nobody has processed it yet" — collapsing the two erases that distinction at the point where it matters most.

## Considered Options

* **Two distinct first-class objects with distinct entry conditions** — manual task / tracked incident for exhausted fulfillment-step remediation; dead-letter for exhausted inbound-delivery retries
* **A single unified "failure record" object with a type discriminator**
* **Route exhausted fulfillment-step remediation into dead-letter as well**

## Decision Outcome

Chosen option: "Two distinct first-class objects with distinct entry conditions", because the two
failure classes have different producers, different retry semantics, and — most importantly —
because a fulfillment step exhausting remediation **already has** its inspectable object (the
manual task under the remediation policy, or the tracked incident under fail-fast, per PRD §6.4) and
**must not grow a second one**. Dead-letter is reserved for **inbound** Lifecycle triggers and
Subscriptions/Payments/Generic-Approval **callbacks** that keep failing past a finite delivery-count
cap; it carries `orderId`, `orderVersion`, the process `correlationId`, the source event/callback
id, and the last error, with an alert to the fulfillment-operator queue. A dead-letter is **never**
an order state. A compensating action that keeps throwing lands on the same incident/manual-task
path already required when compensation cannot complete (per ADR-0005's compensable/no-pivot
classification), never on a silent retry loop and never as a fresh dead-letter entry.

### Consequences

* Workflow must implement two separate stores (or two separate row kinds with non-overlapping entry conditions) — manual task / tracked incident, and dead-letter — each with its own creation trigger, so a failure can never accidentally satisfy both.
* A fulfillment step's exhausted-remediation path is fully closed by the manual-task/incident object; no code path may additionally enqueue a dead-letter row for the same step failure.
* The dead-letter path is exclusively wired to inbound delivery (Lifecycle triggers) and callback delivery (Subscriptions/Payments/Generic-Approval), each with its own finite delivery-count cap; a step-level failure can never reach it by construction, only a delivery-level one.
* A compensating action that keeps throwing must be detectable as "compensation cannot complete" and routed to the existing incident/manual-task path, which means the saga's compensation executor must distinguish "compensation failed and will be retried" from "compensation has exhausted retries" and only the latter opens (or reuses) the incident/manual-task object.
* The dead-letter store is **inbound only in both directions of the rule**: an *outbound* process event that exhausts its publication cap is parked on its own `owf_event_outbox` row (`dead_lettered_at`) and alerted, never written to `owf_dead_letter_record`, whose `source` column has no legal value for a publication this gear originated. ADR-0008 states the same boundary from the outbox side; the two ADRs agree that there is one dead-letter store and it holds only failed deliveries *into* this gear.
* Operator tooling can rely on object type alone to route: manual task and tracked incident surfaces go to the fulfillment-operator remediation queue; dead-letter surfaces go to the same queue but as a delivery-poisoning alert, not a remediation task — the two must remain visually and operationally distinguishable.

### Confirmation

Verified by a test asserting no code path creates both a manual task/incident and a dead-letter
record for the same fulfillment-step failure; by a test asserting dead-letter creation is reachable
only from the inbound-trigger and callback-delivery code paths, never from step-remediation
exhaustion; by a test asserting a dead-letter record can never carry a value in an order-state
field; and by a test asserting a compensating action that exceeds its retry bound opens or reuses
the existing incident/manual-task object rather than looping or creating a dead-letter row.

## Pros and Cons of the Options

### Two distinct first-class objects with distinct entry conditions (chosen)

* Good, because it preserves the 100%-manual-task-for-permanently-failed-lines guarantee without a competing object diluting it.
* Good, because operators can distinguish "a step needs remediation" from "a delivery kept failing" by object type alone.
* Good, because it matches the PRD's explicit rule that a step exhausting remediation must not grow a second inspectable object.
* Neutral, because it requires maintaining two schemas/stores instead of one.
* Bad, because two nearly-adjacent concepts (both are "parked, inspectable failures") require developers to know which one applies in each case.

### A single unified "failure record" object with a type discriminator

* Good, because it reduces the number of tables/queues operators and developers must know about.
* Bad, because a shared schema invites a step-remediation failure and a delivery failure to be handled by the same code path, which is exactly the collapse the PRD rule forbids.
* Bad, because "is this an order state" becomes a per-row question instead of a per-object-type invariant, weakening the "dead-letter is never an order state" guarantee.

### Route exhausted fulfillment-step remediation into dead-letter as well

* Good, because it would reuse existing dead-letter alerting infrastructure for step failures.
* Bad, because it directly violates the PRD rule that a step exhausting remediation already has an inspectable object and must not grow a second one.
* Bad, because dead-letter's re-drive semantics (republish a poisoned delivery) do not map onto "remediate a failed fulfillment step," so reuse would be semantic, not just structural.

## More Information

This decision builds on ADR-0005's saga compensable/no-pivot classification: a compensating action
that cannot complete already escalates to a manual task with the order left non-terminal under
ADR-0005; this ADR fixes that the same object is reused rather than a dead-letter row being opened
in addition.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-workflow-fr-owf-dead-letter` — fixes dead-letter's scope to inbound triggers and callback delivery exhaustion only, fixes that a fulfillment step's remediation-exhaustion object must not be duplicated, and fixes that a dead-letter is never an order state
* `cpt-cf-bss-orders-workflow-nfr-owf-manual-task-sla` — protects the 100%-manual-task, zero-silent-failure guarantee from dilution by a competing dead-letter object for the same step failure
* `cpt-cf-bss-orders-workflow-fr-owf-intent-sweep` — fixes that a terminal failure the sweep discovers routes to the dead-letter path only when it is a delivery-exhaustion case, not a step-remediation case
* `cpt-cf-bss-orders-workflow-component-manual-tasks` (**manual task creator**) — sole creator of the manual-task object under the remediation policy; fulfillment orchestration hands a permanently failed line to it and never writes the task itself
* `cpt-cf-bss-orders-workflow-component-manual-tasks` (**incident recorder**) — creates the non-actionable tracked incident under fail-fast, the alternative object to the manual task and never a second one alongside it
* `cpt-cf-bss-orders-workflow-component-foundation` (**step executor**) — sole writer of `owf_dead_letter_record`, on inbound-delivery or callback cap exhaustion only; the dead-letter store is engine-side and is not owned by any fulfillment or manual-task component
* `cpt-cf-bss-orders-workflow-component-manual-tasks` (**operator task queue**) — surfaces both object types to operators while keeping them visually and operationally distinguishable, which is the entry-condition separation made visible
