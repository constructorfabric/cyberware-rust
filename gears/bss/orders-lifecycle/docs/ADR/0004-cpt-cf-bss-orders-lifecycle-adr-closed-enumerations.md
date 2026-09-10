---
status: accepted
date: 2026-09-09
decision-makers: BSS Orders team
---

# ADR-0004: Both Enumerations Stay Closed — New Distinctions Are Guards, Facts And Reasons

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Keep both sets closed (chosen)](#keep-both-sets-closed-chosen)
  - [Enlarge the sets](#enlarge-the-sets)
  - [Enlarge only the event set](#enlarge-only-the-event-set)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-lifecycle-adr-closed-enumerations`

## Context and Problem Statement

The PRD enumerates **eleven order states** (§6.1) and **eleven state events** (§6.5). Four
distinct design problems each presented as a candidate for enlarging one of those sets:

* Five operations the slices specify had no transition row, among them an order whose required line date is absent — a natural candidate for a twelfth, date-incomplete state.
* An abandoned draft has to reach a terminal state, and "expired" already meant something else.
* Six transition-row classes commit real state changes and have no event in the PRD's set.
* Self-service submit *constitutes* customer acceptance, so one commit carries two facts a consumer might want as two events.

Should either enumeration grow, or should the new distinctions be expressed some other way?

## Decision Drivers

* The event set is a published contract consumed by three gears — Orders Workflow, Subscriptions and the billing chain.
* Adding a state or an event type is **additive and non-breaking for consumers** under `01 §4.6`, conditional on the forward-compatibility obligation stated there — but it still requires an **engine change** and a **PRD amendment**, because both sets are enumerated in PRD §6.1 and §6.5. The cost is process, not consumer migration.
* Every state costs a TTL, a permitted-edge set, guards, and a place in the transition table, whether or not it carries commercial meaning.
* A consumer keyed on the state or event enumeration is the party who pays for a change, and none of them asked for one.
* The programme has direct precedent: `subscriptions/ADR-0001` keeps the manifest status enum closed and expresses trials, pause and intents as attributes.

## Considered Options

* **Keep both sets closed** and express new distinctions as guards, recorded facts, reasons and event-less rows
* **Enlarge the sets** — a twelfth state for date-incomplete orders and for draft auto-void, and new event types for the six event-less row classes
* **Enlarge only the event set**, keeping the states closed

## Decision Outcome

Chosen option: **keep both sets closed**. Concretely:

* A missing required line date is a **gate refusal** with its own reason, not a state (`DECISIONS.md` D-60).
* Draft auto-void targets the existing `expired`, with the payload distinguishing the two facts (D-14).
* Six row classes are **deliberately event-less**, because in each the caller caused the transition and already knows (D-15).
* Self-service acceptance is a **fact recorded inside the submit commit**, publishing only `OrderSubmitted` (D-16).

### Consequences

* **The state machine stays at eleven and the event set at eleven**, so none of the four problems needs an engine change or a PRD amendment. Consumer migration was never the cost — `01 §4.6` makes an addition additive — so what this decision buys is avoiding the engine and PRD churn, not avoiding a breaking change.
* **The outbox cardinality rule stays enforceable.** An earlier draft of this ADR claimed `orders_event_outbox.event_type` is nullable and demoted the rule to a tested invariant on that basis. That was wrong twice: `01 §3.7` declares the column `text` with no nullable marker, and the enqueue is conditional — `01 §3.6` writes a row only *if the transition row declares an event type* — so an event-less transition produces **no** outbox row and no row can ever carry a null type. The rule is therefore "exactly one outbox row per **event-declaring** committed transition", and that remains a checkable cardinality property. The nullability that D-15 introduced is on the in-code transition table's `event_type` declaration, not on the outbox.
* **The event-less justification depends on an external document.** "The caller caused the transition and already knows" is true because the sibling Orders Workflow PRD's trigger set contains neither `submitted → pending_approval` nor `approved → in_fulfillment`. If that trigger set changes, six rows silently need re-examination — so this ADR is a dependency of that PRD, not merely a reader of it.
* **A future consumer that is not the caller cannot observe six transition classes at all**: order creation, draft mutation, the administrative edit, `submitted → pending_approval`, `approved → in_fulfillment`, and the spawn signal. Analytics, an operator timeline and a reconciliation job are all plausibly such consumers. Each would need an engine change plus a PRD amendment, which is the cost this decision defers rather than removes.
* **`OrderExpired` carries two commercially different facts** — a committed order whose TTL lapsed, and a basket never submitted. Consumers must read the payload to tell them apart.
* The `category` enum is documented as **open to a third value** (Q-01) rather than closed, so this decision is about the state and event sets specifically, not about every enumeration in the gear.

### Confirmation

Verified by `01 §4.4`'s event catalogue mapping all eleven events to emitting rows and naming the
six event-less row classes with a justification each; by `01 §4.3` holding twenty-five rows across
eleven states; and by a test asserting no transition is admissible outside the transition table.

## Pros and Cons of the Options

### Keep both sets closed (chosen)

* Good, because no downstream consumer is disturbed by four internal design problems.
* Good, because a guard, a reason or a recorded fact is cheaper than a state in every dimension — no TTL, no edges, no event.
* Good, because it matches `subscriptions/ADR-0001`, so the programme reads one pattern.
* Bad, because six transition classes are unobservable to any consumer that is not the caller.
* Bad, because the transition table needs a nullable event-type declaration to express an event-less row, so "does this row emit?" becomes data rather than structure.

### Enlarge the sets

* Good, because every state change would be externally observable, with no "caller already knows" argument to maintain.
* Good, because a date-incomplete state would let an order rest visibly rather than be refused.
* Bad, because each addition needs an engine change plus a PRD amendment, for consumers who did not ask for the distinction.
* Bad, because a twelfth state needs its own TTL, guards and edges to represent a missing field.

### Enlarge only the event set

* Good, because it addresses the observability cost without touching the state machine.
* Bad, because it still enlarges a three-consumer contract for transitions whose caller already knows the outcome.
* Bad, because it leaves the date-incomplete and auto-void problems unsolved, so a second decision would still be needed.

## More Information

Superseded by nothing. The programme precedent is
`gears/bss/subscriptions/docs/ADR/0001` — the same shape of decision for the subscription status
enumeration. The four register entries this ADR consolidates (D-14, D-15, D-16, D-60) remain in
place and cite it, the way D-01 cites ADR-0001.

## Traceability

- **Governing requirements**: `cpt-cf-bss-orders-lifecycle-fr-order-state-machine`, `cpt-cf-bss-orders-lifecycle-fr-order-events`

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-lifecycle-fr-order-state-machine` — the eleven states stay closed, so four candidate twelfth states become guards, reasons and recorded facts instead
* `cpt-cf-bss-orders-lifecycle-fr-order-events` — the eleven event types stay closed, which is what keeps a three-consumer published contract stable; the cost is six transition classes no non-caller can observe
* `cpt-cf-bss-orders-lifecycle-fr-order-line-dates` — a missing policy-required date is a gate refusal rather than a date-incomplete state, which is D-60's half of this decision
* `cpt-cf-bss-orders-lifecycle-component-transition-engine` — §4.3's row count and §4.4's event catalogue are the enumerations this decision closes, and §4.6 states what adding to either would cost
- **PRD**: [`../PRD.md`](../PRD.md) — §6.1 state machine, §6.5 events, §15 rows 5 and 8
- **DESIGN**: [`../design/01-foundation.md`](../design/01-foundation.md) §4.3, §4.4, §4.6; [`../design/02-capture.md`](../design/02-capture.md) §4.2; [`../design/07-hold-and-expiry.md`](../design/07-hold-and-expiry.md) §4.4
- **Decisions register**: [`../DECISIONS.md`](../DECISIONS.md) — D-14, D-15, D-16, D-60
