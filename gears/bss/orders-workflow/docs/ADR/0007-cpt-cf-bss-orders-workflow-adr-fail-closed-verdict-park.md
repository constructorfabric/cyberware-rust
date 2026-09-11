---
status: accepted
date: 2026-09-10
decision-makers: BSS Orders team (Architecture)
---
# ADR-0007: An Unobtainable Approval Verdict Parks The Order, It Does Not Fail Open


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Fail-closed park (chosen)](#fail-closed-park-chosen)
  - [Fail open to `approved`](#fail-open-to-approved)
  - [Auto-reject the order](#auto-reject-the-order)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-workflow-adr-fail-closed-verdict-park`
## Context and Problem Statement

On `OrderSubmitted`, Orders Workflow must obtain the approval-**requirement** verdict from the
Generic Approval service and reflect it into Orders Lifecycle (`submitted → pending_approval` or
`submitted → approved`). That service has no canonical specification anywhere in this repository
today; until it exists, Workflow invokes a stand-in behind the same PRD §9.2 expectations contract,
and after it exists the service can be unavailable when the verdict is needed. What must Workflow
do with the order when the verdict cannot be obtained?

## Decision Drivers

* Workflow computes neither the verdict nor the approval requirement — only the policy owner does — so an unobtainable verdict is not a value Workflow is entitled to guess at.
* Orders Lifecycle owns the order and its `submitted` TTL; Workflow acts on the order but does not own it, and must not silently borrow authority it does not hold.
* An order that fails open to `approved` without a verdict defeats the entire reason multi-party approval gating exists (financial authorization, legal review, partner sign-off).
* An order that auto-rejects on a transient outage converts an infrastructure problem into a commercial refusal the customer did not cause.
* The `submitted` TTL keeps running regardless of what Workflow does, so whatever is chosen must still escalate before that TTL elapses.
* Until the Generic Approval service exists, some deciding authority must still be named, or every order would stall on a decision nobody is making.

## Considered Options

* **Fail-closed park** — the process parks with the order remaining `submitted`; escalate to the operator queue before the Lifecycle TTL elapses
* **Fail open to `approved`**
* **Auto-reject the order**

## Decision Outcome

Chosen option: "Fail-closed park", because it is the only option that neither asserts a verdict
nobody computed nor manufactures a commercial refusal nobody intended. Until the Generic Approval
service exists, the stand-in behind the same PRD §9.2 expectations contract is the deciding
authority of record and returns `approval not required`; every such reflection is audited as
stand-in, not as a Generic Approval verdict. After the service exists, its unavailability parks the
process in the `parked` phase with the order remaining `submitted` — Workflow **MUST NOT**
fail-open to `approved`. The park **does not suspend** the Lifecycle `submitted` TTL: expiry remains the bound (Lifecycle §6.3),
so Workflow **MUST** escalate to the fulfillment-operator / operator queue **before** that TTL
elapses. Separately, once a gate is already open (`pending_approval`), an outage **pauses that
gate's escalation timer** so the escalation window does not burn into a dead dependency while the
service is down; if the outage outlasts the escalation threshold recorded in
Consequences below, Workflow escalates to the operator queue without issuing the escalation
command through the unavailable service, and still
must not fail-open to `approved` or auto-reject the gate.

### Consequences

* Workflow must persist a distinct "verdict unobtainable" park state, separate from `pending_approval`, since the order has not yet reached a gate — the pre-gate park and the post-gate pause are recorded and escalated by different clocks (Lifecycle TTL vs. gate escalation timer). That state is the process-phase value **`parked`**, one of `started | suspended | parked | compensating | terminated` on the process instance. It is a *process* phase, not an order state: the order itself stays `submitted` throughout, and no new Lifecycle state is introduced. `parked` is distinct from `suspended`, which records an operator- or caller-initiated hold, and a process may not be in both.
* Workflow must implement operator-queue escalation as a first-class path that fires ahead of the Lifecycle `submitted` TTL, with enough lead time to be actionable rather than merely alerting at expiry. Both quantities are stated **relationally** against a configured `lifecycle_submitted_ttl`, because a number fixed on this side would assume a TTL this gear does not own and cannot read from the order:
  * **Outage escalation threshold** — how long the verdict may stay unobtainable before the parked process becomes an operator-visible incident: `min(30 min, 0.25 × lifecycle_submitted_ttl)`. **Accepted.**
  * **Escalation lead time before the `submitted` TTL** — the margin by which the escalation must precede expiry: `max(4 h, 0.25 × lifecycle_submitted_ttl)`. **Accepted.**
  * `lifecycle_submitted_ttl` is read from configuration, not inferred, and its presence and plausibility are **asserted at startup** — a missing or non-positive value, or one smaller than the lead time it must exceed, refuses the configuration rather than being silently defaulted. Orders Lifecycle exposing that value is an open upstream ask (`UPSTREAM_REQS.md`, Orders Lifecycle); until it lands the assertion fails closed and the escalation path is configured, not operating.
* Every stand-in reflection must be tagged as stand-in in the audit trail, so a later cutover to the real Generic Approval service can be distinguished from genuine verdicts in historical data.
* The gate-pause mechanism must reuse the same pause primitive as hold, so escalation-timer arithmetic does not silently include downtime as elapsed window.
* There is deliberately **no in-band un-park authority**: no operator surface force-approves a parked order, because that would be the fail-open this decision refuses, reintroduced through an endpoint. A park is left only by a verdict arriving, by the Lifecycle TTL expiring, or by a workflow-mediated cancel. The absence is recorded here so it reads as a decision rather than an omission.
* No new "approved-without-verdict" or "auto-rejected-without-verdict" order state is ever introduced, which keeps Lifecycle's state machine exactly as documented in its own PRD.

### Confirmation

Verified by a design/code review confirming the pre-gate park state is distinct from
`pending_approval`, that no code path transitions `submitted → approved` without a recorded verdict
(stand-in or real), and that the operator-queue escalation fires with a lead time strictly before
the Lifecycle `submitted` TTL under a simulated verdict-unobtainable condition; and by a test
asserting a gate-open outage pauses the escalation timer rather than letting it elapse; and by a startup test asserting a configuration with an absent, non-positive, or too-short `lifecycle_submitted_ttl` is refused rather than defaulted.

## Pros and Cons of the Options

### Fail-closed park (chosen)

* Good, because it never asserts a verdict nobody computed, preserving the approval requirement as a real gate rather than a formality.
* Good, because it does not manufacture a commercial refusal out of an infrastructure outage.
* Good, because it composes with the existing Lifecycle TTL and gate-pause mechanisms rather than inventing a third clock.
* Neutral, because it requires a park state that is operator-visible before the TTL elapses, which is new surface area.
* Bad, because it makes the order-taking flow depend on operator responsiveness during an outage window.

### Fail open to `approved`

* Good, because it keeps orders moving during an outage, with no visible customer-facing delay.
* Bad, because it grants approval nobody decided, defeating the purpose of financial/legal/partner gating.
* Bad, because a later real verdict of "required, and would have been rejected" cannot be un-fulfilled once fulfillment has started.

### Auto-reject the order

* Good, because it is unambiguous and requires no park state.
* Bad, because it converts a transient infrastructure outage into a permanent commercial refusal the customer did not cause.
* Bad, because it discards legitimate orders that would have cleared approval once the service or stand-in became reachable again.

## More Information

The stand-in's `approval not required` behavior is not a second policy author — it answers behind
the identical PRD §9.2 expectations contract the real Generic Approval service will satisfy, so
swapping the stand-in for the real service requires no change to Workflow's reflection logic.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-workflow-fr-owf-approval-request` — fixes the fail-closed park behavior and the stand-in's role as the deciding authority until the Generic Approval service exists
* `cpt-cf-bss-orders-workflow-fr-owf-approval-escalation` — fixes that a gate-open outage pauses the escalation timer rather than burning the window, and that a prolonged outage escalates to the operator queue without failing open or auto-rejecting
* `cpt-cf-bss-orders-workflow-nfr-owf-escalation-timer` — the ± 5 minute accuracy requirement applies to the same timer this decision requires to be pausable across an outage
* `cpt-cf-bss-orders-workflow-component-approval-execution` (**verdict gateway**) — the single call site for the verdict query and the Lifecycle reflection it drives; it is this component that parks the process in `parked` when the verdict cannot be obtained, never fulfillment orchestration
* `cpt-cf-bss-orders-workflow-component-approval-execution` (**escalation timer owner**) — owns the per-gate escalation timer this decision requires to be pausable across an outage, and fires the pre-TTL escalation
* `cpt-cf-bss-orders-workflow-component-manual-tasks` (**operator task queue**) — the destination surface for the operator-queue escalation; the park is approval-execution control-plane behavior, not Lifecycle state and not fulfillment-orchestration's
