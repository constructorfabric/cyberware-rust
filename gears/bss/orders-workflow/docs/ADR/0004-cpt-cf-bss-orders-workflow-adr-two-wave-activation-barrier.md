---
status: accepted
date: 2026-09-10
decision-makers: BSS Orders team (Architecture)
---

# ADR-0004: Fulfillment Executes as Two Waves With an All-Lines Activation Barrier

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Two-wave barrier (chosen)](#two-wave-barrier-chosen)
  - [Per-line independent activation](#per-line-independent-activation)
  - [Single-wave direct activation](#single-wave-direct-activation)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-workflow-adr-two-wave-activation-barrier`

## Context and Problem Statement

PRD §6.3 requires fulfillment to run as two phases per the Lifecycle atomic contract: wave 1 creates every order line's subscription in `draft` (not resource-affecting), and wave 2 activates lines only after every wave-1 create has succeeded **and** the expected fulfillment time — `max(now, latest service-activation date among lines)` — has been reached. An order's lines can carry mixed service-activation dates, and atomic fulfillment (Lifecycle §6.1) requires the order to be acknowledged `completed` only when all lines are `activated`, with no partial completion. Nothing in the PRD prevents a design from activating each line independently as its own date arrives; the design must decide whether to allow that, or to gate every activation behind a single order-wide barrier. How should Orders Workflow sequence activation across lines with mixed service-activation dates so that the order's atomic-completion contract holds?

## Decision Drivers

* Atomic fulfillment (Lifecycle §6.1) requires all-or-nothing completion; a line that goes live in isolation is a partial commercial outcome even if it is later joined by the rest.
* Mixed service-activation dates on one order are commercially one purchase — staggering their live activation would let one line bill and provision ahead of siblings the buyer purchased as a single order.
* Compensation before activation (draft-void) is cheap; compensation after activation (activated-cancel) is expensive and visible to the customer (PRD §6.4). A barrier that only releases activation once every create has succeeded keeps the expensive leg reachable only after the cheap leg is fully committed.
* A durable timer, not an external trigger, must gate the wait so that a service-activation date in the future does not require an external system to wake the process (§6.3 Provisioning Intent).
* Independent per-line activation would remove the single barrier check but would require per-line overlap and market-divergence re-checks and per-line compensation ordering, multiplying the surface for a partial-commit defect.

## Considered Options

* **Two-wave barrier**: all lines complete wave 1 (draft-create), then a single gate evaluates `max(now, latest service-activation date)` and releases wave 2 (activation) for every line together
* **Per-line independent activation**: each line activates on its own service-activation date as soon as its own wave-1 create succeeds, independent of sibling lines
* **Single-wave direct activation**: skip the draft stage and activate each line immediately on provisioning, deferring all activation-date handling to Subscriptions

## Decision Outcome

Chosen option: **two-wave barrier**, because it is the only option under which mixed service-activation dates cannot stagger live activation of lines that are commercially one order, and it is the only option that keeps compensation cheap (draft-void) for the entire duration one line's date is still pending. Wave 1 issues a draft-create intent per line; the fulfillment task advances to `draft_created` only on confirmation. The barrier does not evaluate until every line has reached `draft_created` — a permanent wave-1 failure on any line halts the order before any activation intent is dispatched, and never triggers activation for the lines that already succeeded. Once every line is `draft_created`, Workflow computes `max(now, latest service-activation date among lines)` and sets a single durable timer for that instant; wave 2 dispatches an activation intent per line only when both the barrier and the timer have released. No line receives an activation intent while any other line of the order still waits on a future service-activation date.

### Consequences

* Workflow must track wave 1 completion for every line before evaluating the barrier, which requires a plan-level aggregate (not just per-line state) to know when all lines have reached `draft_created`.
* The durable timer for `max(now, latest service-activation date)` must survive process restarts and hold suspension (PRD §6.3 Process Suspension); its wake-up cannot depend on an external callback.
* A wave-1 draft auto-voided before its activation intent (hold, platform TTL, or any other cause) forces a wave-1 rebuild for that line before the barrier can be re-evaluated, since the barrier's precondition is "every line currently in `draft`," not "every line was once in `draft`."
* Future extensibility to per-line SLA reporting must be layered on top of the barrier as an observability concern (progress-read operation), not as a relaxation of the barrier itself — the barrier's atomicity is the property this decision protects.

### Confirmation

Confirmed by a scenario test with three lines carrying distinct service-activation dates (past, near-future, far-future) asserting no activation intent is dispatched until all three reach `draft_created` and the timer for the latest date has elapsed; by a test asserting a wave-1 permanent failure on one line prevents any activation intent for the order's other lines; and by a test asserting a wave-1 draft auto-voided during the date wait triggers a rebuild rather than a stale activation intent.

## Pros and Cons of the Options

### Two-wave barrier (chosen)

* Good, because it makes the atomic-completion contract structural: activation is a single gated event for the whole order, not nine independent races.
* Good, because it keeps the order in the cheap compensation regime (draft-void) for the entire time any line is waiting on a future date.
* Good, because the durable timer and the barrier are one mechanism, not one per line, so there is one place to reason about correctness.
* Neutral, because it means the order's earliest-dated line is delayed until the latest-dated line's date, trading per-line speed for commercial coherence.
* Bad, because a single slow or late-dated line delays activation for every other line in the order, even ones ready long before.

### Per-line independent activation

* Good, because each line could go live as soon as its own date and its own draft-create succeed, without waiting on siblings.
* Bad, because it directly violates atomic fulfillment: one line could be `activated` (billable, resource-affecting) while a sibling is still `pending`, which is a partial commercial outcome the Lifecycle contract forbids.
* Bad, because it multiplies the overlap-presence and market-divergence re-checks (§6.3) from one per order to one per line per activation, and multiplies compensation-ordering complexity if a later line then fails.
* Bad, because mixed dates on the same order become externally visible as staggered service starts for what the buyer purchased as one order.

### Single-wave direct activation

* Good, because it removes the draft stage entirely, simplifying the state machine to one transition per line.
* Bad, because every provisioning intent becomes resource-affecting immediately, so a permanent failure on any line after another has already gone live requires activated-cancel compensation instead of the cheap draft-void, for every prior line.
* Bad, because it discards the explicit two-phase contract the Lifecycle PRD (§6.1) requires fulfillment to honor.

## More Information

This ADR is scoped to the two-wave sequencing decision only; the compensation classification that follows from having two waves is recorded separately in ADR-0005, and the per-intent idempotency-key composition that makes each wave's intents safely retryable is recorded in ADR-0006.

## Traceability

- **PRD**: [PRD.md](../PRD.md) — §6.3 Fulfillment Plan Construction, §6.3 Provisioning Intent to Subscriptions
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-workflow-fr-owf-fulfillment-plan` — this decision is the two-phase execution contract that requirement's frozen plan is executed against.
* `cpt-cf-bss-orders-workflow-fr-owf-provisioning-intent` — this decision is why a durable timer, not Subscriptions, owns the future-dated activation wait, and why no line receives an activation intent while a sibling still waits on its date.
* `cpt-cf-bss-orders-workflow-fr-owf-line-progress` — this decision is why order-level `completed` requires the barrier to have released for every line, matching the atomic-completion requirement.
