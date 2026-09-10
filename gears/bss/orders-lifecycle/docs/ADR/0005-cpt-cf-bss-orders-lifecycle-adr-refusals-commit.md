---
status: accepted
date: 2026-09-09
decision-makers: BSS Orders team
---

# ADR-0005: A Refused Transition Is A Committed Outcome

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Refusals commit (chosen)](#refusals-commit-chosen)
  - [Refusals log to a separate store](#refusals-log-to-a-separate-store)
  - [Audit refusals but do not settle them](#audit-refusals-but-do-not-settle-them)
  - [Audit only authorization denials](#audit-only-authorization-denials)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-lifecycle-adr-refusals-commit`

## Context and Problem Statement

PRD §7.1 requires 100 % of transitions to be audited with **zero silent drops**. Most of a
transition engine's traffic in production is not state changes — it is refusals: an unauthorized
caller, a mismatched idempotency key, a request still in flight, an inadmissible `(state, trigger)`
pair, a failed guard.

Is a refusal an *outcome the engine records*, or an *error the engine returns*? The question sounds
semantic and is not: it decides whether the engine writes durable rows on the majority of its
requests.

## Decision Drivers

* "Zero silent drops" is meaningless if the dropped attempts are the unaudited ones.
* A refused submit that leaves no trace is not replayable, so a caller cannot distinguish "refused" from "never arrived".
* An auditor investigating a disputed order needs the attempts that failed, not only the ones that worked — a denied authorization is the most interesting row in the trail.
* Whatever is chosen applies to every refusal class uniformly, or the guarantee becomes a per-class argument.
* Writing on refusal is a write-amplification vector, and a refusal loop is cheap for a caller to create.

## Considered Options

* **Refusals commit** — every refusal path appends an audit entry, settles its idempotency record where one exists, and commits before returning
* **Refusals log** — audit refusals to a separate, non-transactional log outside the order's trail
* **Audit refusals but do not settle them** — record the attempt, leave the idempotency key unsettled so a retry re-attempts
* **Audit only authorization denials**, treating guard failures as ordinary validation errors

## Decision Outcome

Chosen option: **refusals commit**. `design/01-foundation.md` §4.1 is normative: every refused
transition appends an audit entry and commits, across **all seven refusal classes without
exception**. A failed audit append aborts the attempt.

**Settlement is narrower than auditing, and deliberately so.** **Four of the seven classes settle**
their idempotency record — unresolvable guard input (`§3.6` step 3.1), not-admissible (11.1),
version conflict (12.1) and failed slice guard (13.2). The other three do not, and in each case
because there is no record it would be correct to settle:

* **Authorization denial** refuses *before* the registry is read (step 1), so no caller-supplied key is consulted. This is the security carve-out: an unauthorized caller must not be able to learn a stored outcome, nor to squat a key ahead of the authorized one.
* **Idempotency-fingerprint mismatch** (7.1.1) means a settled record already exists carrying a *different* request. Re-settling it would destroy the stored outcome the contract promises to replay.
* **Still-processing** (8.1) means the record is in-flight and owned by another request. Settling it would steal that request's marker — and still-processing is not a final answer but "not yet decided", so the property below does not apply to it.

An earlier statement of this decision claimed all seven settled "with one deliberate scoping
caveat", and `01 §4.1` propagated it as "six of the seven". Both overcounted: the algorithm settles
four. The correction does not weaken the decision — every refusal is still audited and committed —
but it matters, because the argument against the rejected third option below is a claim about
*settled* refusals and must not be read as covering classes that have nothing to settle.

The third option — audit but do not settle — was the closest call, and is rejected because it
breaks the property the whole idempotency contract rests on **for the four classes that reach a
decision**. If a decided refusal does not settle, a caller can convert it into a success by
retrying, and the stored-outcome guarantee becomes "the stored outcome, unless it was a refusal".
The three non-settling classes above are not counter-examples: two have a record they must not
overwrite, and the third has none to write. Settling makes a refusal replay *as that refusal*,
which is what lets a client retry safely without a branch on the previous answer.

### Consequences

* **A refusal is a durable write.** On a request mix dominated by refusals, the engine writes on most requests. This is the cost, and it is bounded rather than unbounded: refusal audit rows carry a **90-day retention** distinct from committed transitions, and repeated refusals against one order are **rate-limited** (`DECISIONS.md` D-49). Those two mechanisms exist *because* of this decision.
* **A settled refusal is frozen for the idempotency window.** A transient guard failure — a port that was briefly unavailable, a contract that was momentarily unresolvable — replays as that refusal for **24 hours** under the same key. A caller who wants a genuine retry must use a **new** idempotency key. This is the sharpest caller-visible consequence of the decision and the one most likely to surprise.
* Every refusal reason must be **registered** and machine-readable, since it is persisted and replayed rather than formatted for a human once.
* Slice pre-checks cannot short-circuit ahead of the engine: a check that refuses before the engine is consulted produces no audit row and no settled record, which is why `01 §2.1` requires slice checks to be **registered guards** the engine evaluates.
* The audit store's growth is driven by traffic, not by commercial activity, so its capacity planning and its retention differ from the commercial trail's.
* An auditor can answer "who tried and was refused" as easily as "who succeeded", which is what makes the trail evidence rather than a changelog.

### Confirmation

Verified by the transition algorithm carrying the audit-append and commit sequence on every
refusal branch, and the settle sequence on every refusal reached after authoritative idempotency
resolution (`design/01-foundation.md` §3.6), by a fault-injection test asserting a failed audit
append aborts the transition, and by a test asserting a replayed key whose stored outcome is a
refusal returns that refusal rather than re-attempting. Authorization denial is the deliberate
pre-probe exception: it is audited and refused without consulting or settling a caller-supplied
idempotency key, so an unauthorized caller cannot learn a stored outcome or preempt an authorized
caller’s key.

## Pros and Cons of the Options

### Refusals commit (chosen)

* Good, because "100 % audited, zero silent drops" becomes a property of one code path rather than a discipline.
* Good, because a refused attempt is replayable and diagnosable.
* Good, because every refusal is audited and committed, so no refusal class can disappear silently.
* Bad, because the engine writes on the majority of requests, needing a separate retention and a rate limit.
* Bad, because a transient failure is frozen under its key for 24 hours, which callers must understand.

### Refusals log to a separate store

* Good, because the order's trail stays purely commercial and small.
* Good, because logging is cheap and needs no transaction.
* Bad, because a non-transactional log *can* drop, so "zero silent drops" is no longer assertable.
* Bad, because the auditor now needs two stores and a correlation between them to see one order's history.

### Audit refusals but do not settle them

* Good, because a transient failure is retryable under the same key.
* Good, because it still gives complete audit coverage.
* Bad, because a caller can turn a refusal into a success by retrying, so the stored-outcome contract acquires an exception.
* Bad, because a retry storm against a failing guard writes an audit row per attempt with no de-duplication.

### Audit only authorization denials

* Good, because it captures the security-relevant subset at the lowest cost.
* Bad, because a failed sellability guard is exactly what a disputed order turns on, and it would be invisible.
* Bad, because "100 %" would have to be restated as "100 % of a named subset".

## More Information

Superseded by nothing. This decision is why ADR-0001's four-durable-effects consequence reads
"the state **or version** change" — on a refusal there is no state change, and the other three
effects still occur. The two register entries it consolidates (D-05, D-08) remain in place and
cite it; D-49 records the retention and rate limit that bound its cost.

## Traceability

- **Governing requirements**: `cpt-cf-bss-orders-lifecycle-nfr-order-audit-completeness`, `cpt-cf-bss-orders-lifecycle-fr-order-idempotency`

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-lifecycle-nfr-order-audit-completeness` — "zero silent drops" is met by auditing the refusals too, which are the majority of production traffic and the attempts an auditor most wants
* `cpt-cf-bss-orders-lifecycle-fr-order-idempotency` — a settled refusal replays as that refusal, which is what lets a caller retry without branching on the previous answer; the four classes that settle and the three that cannot are enumerated in the Decision Outcome
* `cpt-cf-bss-orders-lifecycle-component-gate-and-pin` — a refused submit is replayable and diagnosable, so a caller can distinguish "refused" from "never arrived"
* `cpt-cf-bss-orders-lifecycle-component-transition-engine` — the engine writes on the majority of its requests as a direct result, which is why the refusal retention and the rate limit exist and why the retention needs a worker behind it
- **PRD**: [`../PRD.md`](../PRD.md) — §7.1 audit completeness, §12 show-stopper 19
- **DESIGN**: [`../design/01-foundation.md`](../design/01-foundation.md) §2.1, §3.6, §4.1, §4.2
- **Decisions register**: [`../DECISIONS.md`](../DECISIONS.md) — D-05, D-08, D-49
- **Related ADR**: [`./0001-cpt-cf-bss-orders-lifecycle-adr-transition-through-engine.md`](./0001-cpt-cf-bss-orders-lifecycle-adr-transition-through-engine.md)
