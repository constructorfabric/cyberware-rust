---
status: accepted
date: 2026-09-10
decision-makers: BSS Orders team (Architecture)
---

# ADR-0005: Both Fulfillment Waves Are Compensable; This Phase Has No Intra-Saga Pivot

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Both waves compensable, no pivot (chosen)](#both-waves-compensable-no-pivot-chosen)
  - [Activation as a pivot point](#activation-as-a-pivot-point)
  - [Forward-only remediation with no compensation for wave 2](#forward-only-remediation-with-no-compensation-for-wave-2)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-workflow-adr-saga-compensable-no-pivot`

## Context and Problem Statement

PRD §6.4 requires every fulfillment step to be classified at design time as compensable or irreversible, with a compensable step declaring its compensating action, and states explicitly that this phase's two waves (ADR-0004) are both compensable — draft-void for a completed wave-1 create, activated-cancel for a completed wave-2 activation — with no intra-saga pivot: activation does not make rollback impossible, it changes which compensating action applies. A saga design that treated activation as a pivot point would need a third, irreversible leg and would have to reason about a class of failure this gear never actually faces in this phase. How should the design classify the two waves and order their compensation so the blanket "compensable" statement in the PRD is realized rather than merely asserted, and so a compensating action that itself cannot complete has one defined outcome rather than an invented one?

## Decision Drivers

* PRD §6.4 fixes the classification (both waves compensable) and the no-pivot statement; the design's job is to make that operationally real, not to re-derive it.
* Atomic fulfillment (Lifecycle §6.1) means no created subscription may survive a failed or cancelled order — compensation must reach every subscription created for the order, not just the one that failed.
* Draft-void and activated-cancel are asymmetric in cost and visibility: voiding a draft has no billable facts; cancelling an activated subscription is resource-affecting and customer-visible. The design must keep that asymmetry explicit rather than treating both legs as one undifferentiated "rollback."
* Unwinding in the reverse of forward execution bounds the blast radius to exactly what was provisioned, and is the only order in which dependent-line provisioning relationships (Catalog topology, §6.3) can be safely unwound. Because forward execution is wave-ordered — every wave-1 create precedes every wave-2 activation — reversing it unwinds the whole activated set before any draft in the normal case, without needing a separate wave rule to say so.
* A compensating action can itself fail (a draft that cannot be voided, an activated subscription that cannot be cancelled) — the design must give that failure exactly one destination rather than a bespoke retry loop or a fabricated third compensating action.
* The two waves are not the only registered steps with a durable external effect: `evaluate-payment-auth-eligibility` and the `begin-fulfillment` Lifecycle transition sit outside both waves, and PRD §6.4 requires *every* fulfillment step to be classified at design time. A classification that covers only the waves leaves the no-pivot claim unverified rather than true.
* OSS-side irreversibility is out of scope for this gear: Orders Workflow never calls OSS directly (R3), so any irreversibility at the OSS layer is absorbed by Subscriptions' own cancel semantics, not by a compensating action this gear invents.

## Considered Options

* **Both waves compensable, no pivot**: draft-void compensates any completed wave-1 create, activated-cancel compensates any completed wave-2 activation; compensation unwinds the strict reverse of recorded forward execution order, which for a plain order takes the activated set before the draft set; an incompletable compensation escalates to a manual task with the order left non-terminal
* **Activation as a pivot point**: treat wave-2 activation as the saga's point of no return, after which failure of any other line is handled by a forward-recovery/retry-only path rather than by rolling the activated line back
* **Forward-only remediation with no compensation for wave 2**: never issue activated-cancel; an activation-phase failure is always resolved by manual override or retry, never by undoing an already-activated line

## Decision Outcome

Chosen option: **both waves compensable, no pivot**, because it is the only option that satisfies the PRD's explicit classification and lets one uniform mechanism — reverse-order compensation dispatched as provisioning intents under the same identity and idempotency contract as forward execution (ADR-0006) — cover both legs. On order-level fulfillment failure or an authorized workflow cancellation, Workflow compensates every subscription created for the order by unwinding the reverse of forward execution. The ordering rule is stated once, as one rule, with no second formulation anywhere:

**Compensation subjects are walked in strict reverse of their recorded forward execution order**, descending the persisted forward-execution ordinal. That is the whole rule.

For a plain order it yields the wave-grouped sequence — every activated line cancelled before any draft is voided — because the two-wave barrier guarantees every wave-1 create precedes every wave-2 activation, so reversing execution order reverses the waves with it. Wave grouping is therefore a *description of the normal-case result*, not an independent second level, and it is deliberately not stated as a rule that could override the first.

The distinction matters where a create is accepted **after** an activation, which the barrier permits in exactly two cases: a wave-1 rebuild after a draft is auto-voided, and a subscription attached late by a verified operator override. There, reverse execution order and wave grouping disagree, and reverse execution order governs — because the property the walk exists to preserve is dependency-safe unwind, a dependent removed before the dependency it was built on, and that property is carried by execution order, not by wave membership. Wave grouping in those cases would cancel the activated line first and then void a draft that was built on it, which is the unsafe direction and contradicts the driver above. A draft that cannot be voided and an activated subscription that cannot be cancelled are distinct failures with distinct manual-task reasons; neither is retried indefinitely as a silent loop. If a compensating action cannot complete, Workflow does not report the order as compensated — the order remains non-terminal, a manual task is created, and Workflow escalates until operational compensation reaches a known outcome (bounded by the same overdue-fulfillment SLA as forward execution, §6.3). This is a deliberate two-outcome design: either compensation completes and the order reaches a terminal outcome (`fulfillment_failed` or `cancelled`), or it does not and the order stays non-terminal under an escalated manual task — there is no third, invented outcome.

### Consequences

* Compensation dispatch must know the forward execution order of every subscription in the order (not just their current per-line state) so it can walk it in reverse. That order must be **persisted, not inferred**: a monotonic forward-execution ordinal, assigned per `(order_id, order_version)` when the intent is dispatched, is stored on the compensation subject alongside its `wave` (the slice realizes this as `owf_compensation_record.compensation_sequence`, fed from the dispatch order of `owf_provisioning_intent`), and the compensation scan reads wave descending, then ordinal descending. Without a persisted ordinal the scan's `(order_id, order_version)` index yields a set rather than a sequence, and the ordering rule above is unimplementable at all — neither a timestamp nor row order is a substitute, since both can tie or be rewritten.
* A compensating action is submitted as a provisioning intent (draft-void or activated-cancel) under the same idempotency-key contract as forward intents (ADR-0006), so compensation inherits the same retry, timeout, and duplicate-submission handling rather than needing its own.
* The manual-task and dead-letter mechanisms (§6.4, §6.3) must accept a compensation-failure reason distinct from a forward-execution failure reason, since operators need to know which leg (void vs. cancel) is stuck.
* Cancellation fencing (stop new intents, reconcile in-flight intents, compensate every created subscription including late successes, verify no active subscription remains) is required before any compensated outcome is reported, because reverse-order compensation is only correct if the set of "created subscriptions" it walks is complete and final.
* The two registered steps outside the waves are classified here, so PRD §6.4's "every fulfillment step MUST be classified" is met rather than assumed:
  * `evaluate-payment-auth-eligibility` is a **read-only guard evaluation**. It produces no durable effect at any external party — it consumes an authorization outcome and gates dispatch — so it declares **no compensating action**, and its absence is not an irreversible leg. It is neither a pivot nor a gap in the classification.
  * The `begin-fulfillment` Lifecycle transition is **compensable**, and its compensating action is the order-level compensated-outcome report this ADR already requires (`fulfillment_failed` or `cancelled`) reflected onto the same Lifecycle seam. It is not a pivot, because reporting the terminal outcome is always available and never blocked by what the waves did.
  * Because neither step is irreversible, the no-pivot claim holds over the whole registered step set and not merely over the two waves.
* This decision forecloses building a forward-recovery-only path for activation failures in this phase; a future phase that needs one would require a new ADR, not a silent extension of this one.

### Confirmation

Confirmed by a saga test that provisions three lines (two activated, one still draft, the draft created *after* both activations via a wave-1 rebuild), fails the order, and asserts the walk follows descending `execution_seq` — so the later-created draft is voided *before* the two activated lines are cancelled, and a dependent is never left behind its dependency; by a second test on a plain order (no rebuild) asserting the same rule yields the wave-grouped sequence, activated set first; by a test asserting the compensation scan reads a persisted `execution_seq` rather than deriving order from a timestamp or from row order; by a test asserting `evaluate-payment-auth-eligibility` dispatches no compensating action and `begin-fulfillment` compensates through the terminal-outcome report; by a test asserting a failed activated-cancel produces a manual task with a cancel-specific reason and leaves the order non-terminal rather than reporting `fulfillment_failed`; and by a test asserting a failed draft-void produces a distinct void-specific manual-task reason.

## Pros and Cons of the Options

### Both waves compensable, no pivot (chosen)

* Good, because it matches the PRD's explicit classification exactly, with no gap between the recorded decision and the implemented behavior.
* Good, because wave-grouped reverse-order compensation is one mechanism that covers both legs, rather than two different rollback strategies either side of an activation pivot.
* Good, because an incompletable compensation has exactly one destination (manual task, order non-terminal), which is testable and auditable.
* Neutral, because it means an already-activated, customer-visible subscription can be cancelled by this gear as a matter of course — that is a deliberate and expected outcome, not an edge case.
* Bad, because activated-cancel is inherently more expensive and more visible than draft-void, and this option accepts paying that cost whenever an order fails after any line has activated.

### Activation as a pivot point

* Good, because it would avoid ever cancelling an already-activated, customer-visible subscription.
* Bad, because it directly contradicts the PRD's explicit no-pivot statement and its stated classification of wave 2 as compensable.
* Bad, because it would need a defined forward-recovery path for every failure mode after activation, effectively a second saga type layered on top of the first, with no PRD requirement driving its design.
* Bad, because atomic fulfillment (Lifecycle §6.1) already requires no created subscription to survive a failed order — a pivot that refuses to unwind an activated line would leave a stranded, billed resource on a failed order.

### Forward-only remediation with no compensation for wave 2

* Good, because it never issues a customer-visible cancel, avoiding that operational cost entirely.
* Bad, because it has no answer for the case remediation cannot resolve — the PRD requires compensation for exactly that case (remediation exhausted or fail-fast), and this option has nothing to invoke.
* Bad, because it violates atomic fulfillment: a failed order with an activated line and no compensating action leaves a stranded, billed subscription behind.

## More Information

This ADR assumes the two-wave structure recorded in ADR-0004 and the provisioning-intent identity/idempotency contract recorded in ADR-0006; both compensating actions (draft-void, activated-cancel) are dispatched as provisioning intents under that same contract, not as a separate compensation-specific call shape.

## Traceability

- **PRD**: [PRD.md](../PRD.md) — §6.4 Per-Step Compensation Declaration, §6.4 Compensation Execution on Permanent Failure
- **DESIGN**: [DESIGN.md](../DESIGN.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-workflow-fr-owf-compensation-declaration` — this decision is the concrete classification (both waves compensable, no pivot) that requirement calls for at design time.
* `cpt-cf-bss-orders-workflow-fr-owf-compensation-execution` — this decision is the reverse-order dispatch rule and the two-outcome (compensated-terminal vs. non-terminal-escalated) resolution that requirement mandates.
* `cpt-cf-bss-orders-workflow-fr-owf-manual-task` — this decision is why a compensation failure produces a manual task with a leg-specific reason rather than a silent retry loop.
