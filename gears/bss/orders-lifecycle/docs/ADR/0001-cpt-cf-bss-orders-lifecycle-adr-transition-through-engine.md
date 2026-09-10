---
status: accepted
date: 2026-09-08
decision-makers: BSS Orders team
---

# ADR-0001: Transition Through The Engine — One Guarded Commit Owns Every State Change


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Per-capability state handling](#per-capability-state-handling)
  - [A shared library of helpers](#a-shared-library-of-helpers)
  - [Transition through one engine (chosen)](#transition-through-one-engine-chosen)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-lifecycle-adr-transition-through-engine`
## Context and Problem Statement

Four of the PRD's `p1` non-functional requirements — 100 % audit completeness, zero duplicate
transition effects, a p95 commit under one second, and zero data loss for `submitted`-or-beyond
orders — are properties of *how a state change commits* rather than of any capability that
requests one. The order document also has eleven states, seven capability areas and two callers
(buyer surfaces and the sibling Orders Workflow gear). Where should the transition mechanics
live, so that those four guarantees are assertable once rather than argued per capability?

## Decision Drivers

* Audit completeness must be structural, not a discipline: an unaudited transition must be unable to commit.
* Duplicate effect must be impossible rather than unlikely, because a duplicate order double-provisions and double-charges.
* The sibling gear must be unable to hold authoritative order state (PRD §6.4 R1), and the mechanism must make that a property rather than a rule someone remembers.
* A new capability must not be able to regress the correctness core.
* The commit path must be free of network calls to keep the latency budget achievable with slow upstreams.
* The two built BSS siblings already solved the analogous problem, and divergence from them costs review effort.

## Considered Options

* Per-capability state handling, with each slice owning its own writes and the guarantees asserted per slice
* A shared library of helpers that slices call, with slices still performing their own writes
* Transition through one engine: a single guarded, audited, idempotent commit path that is the only writer

## Decision Outcome

Chosen option: **transition through one engine**, because it is the only option under which the
four `p1` guarantees are assertable in one place and unbreakable by a later capability. The
engine owns the aggregate and its append-only version chain, the declarative state-machine
table and guard evaluation, the idempotency registry, the optimistic version check, the
transition audit, the event outbox and the reason registry. Slices declare guard predicates and
supply document contributions; they never write order state, never append an audit row and never
emit an event.

The shape is adopted rather than invented: the Billing Ledger commits balanced journal lines
through one posting engine, and the Product Catalog publishes through one fail-closed validation
engine with an append-only history and an outbox. This is the same pattern applied to a state
machine.

### Consequences

* Every state change enters through one operation and commits four durable effects in one database transaction: the state or version change, one audit entry, one settled idempotency record, and one outbox row where the transition row declares an event.
* No component other than the engine may write the aggregate, version, line, resolved-total, audit, idempotency or outbox tables — including migrations, repair scripts and administrative surfaces. A data-repair need becomes a new transition-table row with its own guard and reason, which is a real operational cost accepted deliberately.
* Guard inputs must be resolved **before** the transaction opens, so no outbound call sits in the commit path. Slices that need external data resolve it through ports and pass plain values. The idempotency registry is nonetheless probed *ahead* of that resolution, so a replay returns its stored outcome without re-invoking any port (D-65).
* **A refusal is also a committed outcome.** Every refused transition — all seven classes enumerated in `01 §4.1`: unresolvable guard input, unauthorized, idempotency mismatch, still-processing, not-admissible, version conflict and slice-guard failure — appends an audit entry, settles its idempotency record where one exists, and commits. Two corollaries follow: a settled refusal *replays as that refusal* for the idempotency window, so a caller cannot convert a refusal into a success by retrying; and refusal auditing is a write-amplification vector, bounded by the 90-day refusal retention and the per-order refusal rate limit of D-49. Recorded in full as [`./0005-cpt-cf-bss-orders-lifecycle-adr-refusals-commit.md`](./0005-cpt-cf-bss-orders-lifecycle-adr-refusals-commit.md).
* **One exception to the single-writer rule exists**: in-place pseudonymisation of actor identifiers to satisfy an erasure obligation is the only permitted mutation of an append-only store, is itself an audited transition, and qualifies the bullet below (D-44).
* Any new capability requiring a state, an edge or an event type is an **engine change**, not a slice change. Slices may add guards, reasons, document contributions and policy rows without touching the engine. This boundary is recorded normatively in `design/01-foundation.md` §4.6.
* Guard evaluation order is fixed and total, so two capabilities cannot disagree about precedence.
* The state machine becomes data — a transition table — which makes edge coverage enumerable and lets normative exclusions (notably the absence of an `in_fulfillment → expired` row) be structural rather than conditional.

### Confirmation

**This gear has no implementation and no runtime tests.** The checks below are therefore split
into what is verifiable today by reading the design set, and what is *planned* and not yet
written. Nothing here cites an existing test of this gear's code.

**Verifiable today, and stated at the scope the documents actually support.**
`design/01-foundation.md` §4.1 makes the four-effects-in-one-transaction rule and the total
guard-evaluation order normative. §2.2's single-writer constraint names **seven table families** —
aggregate, version, line, resolved-total, audit, idempotency and outbox — and bars any migration,
repair script, administrative surface or slice from writing them outside a transition. It is not a
claim about all nineteen tables in the set, and two are deliberately outside it:
`orders_draft_content` is declared mutable and is written by capture, and `orders_read_access_log`
is written by the read surface (`08 §3.7`). Reading it as "every table" would be an overclaim, and
the seven it does name are the ones the audit guarantee rests on. §3.6 *Attempt Transition* carries
an audit append and a commit on **every branch that decides an attempt** — all seven refusal
classes included; the one branch that commits without appending is the **replay** of a settled
record, which records no new attempt because none occurred. §4.3 holds the state
machine as data — twenty-five rows over eleven states — which is what makes edge coverage
enumerable. The document-level invariant suite (`scripts/check-design-invariants.py`, run as
`make design-check` in CI) mechanises the cross-document part of that: among its families are the
assertion that no slice algorithm returns a refusal ahead of its engine call, and the assertion
that no §4.3 row expires from `in_fulfillment` — the normative exclusion this decision makes
structural. Those families hold over the *documents*; they assert nothing about code.

**Planned, not yet written.** The runtime checks belong to the NFRs they serve and are recorded
as the Verification Approach column of `01 §1.2`. From the **audit-completeness** row: a
structural check that every transition-table row writes an audit entry on both outcomes, a
fault-injection check that a failed audit append aborts the transition, and a periodic
chain-verification job. From the **idempotency** row: a parallel same-key concurrency check
asserting exactly one durable effect, a replay check asserting a stored failure replays as a
failure, and a crash check asserting an expired lease is recoverable.

**Planned, and not yet specified anywhere.** An **edge-coverage check** asserting no transition
is admissible at runtime outside the twenty-five rows of §4.3 has no home in the design set —
no document states it and nothing implements it. Until `01 §1.2` records it alongside the other
verification approaches, the normative exclusions this decision makes structural — notably the
absent `in_fulfillment → expired` row — rest on the transition table plus the single-writer grant
at design level, and on the invariant suite's document-level assertion above, not on any
mechanised runtime check.

## Pros and Cons of the Options

### Per-capability state handling

* Good, because a slice is independently deployable in principle and needs no shared component.
* Good, because a capability author touches one file.
* Bad, because each of the four `p1` guarantees must be re-argued per slice, and the audit guarantee in particular becomes a review discipline rather than a property.
* Bad, because nothing prevents a later slice from writing state without a guard, so R1 degrades silently.
* Bad, because the idempotency contract would be implemented several times and would diverge.

### A shared library of helpers

* Good, because it reduces duplication without introducing a chokepoint.
* Good, because slices keep local control over their own transactions.
* Bad, because a helper is optional by construction: nothing stops a slice bypassing it, so the guarantees remain unenforceable.
* Bad, because transaction boundaries stay slice-owned, so "one transaction, four effects" cannot be asserted at all.

### Transition through one engine (chosen)

* Good, because the four guarantees are properties of one code path and are asserted once.
* Good, because R1 becomes structural: there is no code path by which any caller can set state without passing a guard.
* Good, because it matches the two built sibling gears, so reviewers already know the shape.
* Bad, because the engine is a deliberate chokepoint: every new state, edge or event type is an engine change, and the design must say so rather than implying open extensibility.
* Bad, because slices lose the freedom to shape their own transactions, and pre-engine validation must be expressed as registered guards rather than as inline checks.

## More Information

Superseded by nothing. The rejected alternatives were re-examined during the 2026-09-08 design
review wave, which found that the engine's *contract* was correct while its algorithm was not —
the ordering defects recorded as `R-01` through `R-03` in
the 2026-09-08 review wave were
implementation-level and did not disturb this decision.

## Traceability

- **Governing requirements**: `cpt-cf-bss-orders-lifecycle-fr-order-state-machine`, `cpt-cf-bss-orders-lifecycle-fr-order-idempotency`, `cpt-cf-bss-orders-lifecycle-nfr-order-audit-completeness`

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-lifecycle-fr-order-state-machine` — every admissible edge lives in one table the engine owns, so an inadmissible transition is unreachable rather than merely refused by each caller
* `cpt-cf-bss-orders-lifecycle-fr-order-idempotency` — the registry is consulted on one code path, so replay semantics cannot differ between operations
* `cpt-cf-bss-orders-lifecycle-nfr-order-audit-completeness` — the audit append shares the transition transaction, which is what makes "100 %, zero silent drops" a property of one path rather than eight disciplines
* `cpt-cf-bss-orders-lifecycle-fr-order-authorization` — the shared evaluator runs as the engine's pre-guard, so no handler can widen its own scope
* `cpt-cf-bss-orders-lifecycle-component-transition-engine` — this decision is what makes that component the single writer; every other component reaches state through it
- **PRD**: [`../PRD.md`](../PRD.md) — §6.1 state machine and idempotency, §6.4 R1, §7.1
- **DESIGN**: [`../DESIGN.md`](../DESIGN.md) §1.1, §2.1; [`../design/01-foundation.md`](../design/01-foundation.md) §4.1
- **Decisions register**: [`../DECISIONS.md`](../DECISIONS.md) — D-01
