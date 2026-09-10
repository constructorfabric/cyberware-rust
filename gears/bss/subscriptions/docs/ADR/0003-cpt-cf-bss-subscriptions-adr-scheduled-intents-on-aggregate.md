---
status: proposed
date: 2026-07-15
decision-makers: "BSS Subscriptions team"
---

Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

# ADR-0003: Scheduled Intents Live on the Aggregate (Not Portal-Side Automation)

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Portal-side automation (cron re-keys the transition)](#portal-side-automation-cron-re-keys-the-transition)
  - [Pending intents on the aggregate (chosen)](#pending-intents-on-the-aggregate-chosen)
  - [A native SubscriptionSchedule aggregate](#a-native-subscriptionschedule-aggregate)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-subscriptions-adr-scheduled-intents-on-aggregate`

> **Status: proposed / flagged for veto.** Records the rationale for [`../DECISIONS.md`](../DECISIONS.md)
> SUB-D-01 and SUB-D-04; ratified when Product/Architecture confirms.

## Context and Problem Statement

The most common self-service intent is "**cancel at period end**" (Stripe `cancel_at_period_end`,
Zuora cancellation policies); "suspend until a date" and committed multi-step **ramps** (Zuora
Ramps/Orders) are the enterprise equivalents. None of these is an *immediate* transition — they are a
**future** transition the customer commits to **now**. If the future firing is implemented as
portal-side automation (a cron job that re-keys the real transition at the due date), the commitment
is **invisible** to the renewal job, Billing, and audit until it fires: the renewal job may auto-renew
a subscription the customer already asked to cancel, and there is no auditable record of the standing
intent.

Where does a committed-but-future transition live so the renewal job, Billing, and audit can all see
it before it fires?

## Decision Drivers

* The renewal job MUST see a pending end-of-term cancel and **suppress** the renewal + next-term recurring — otherwise it double-acts against the customer's intent.
* Billing and audit need the standing intent as first-class, auditable state — not a hidden scheduler row in a portal.
* A scheduled transition must run the **full guard set** (Policy/OSS) at its effective instant — it is not a pre-authorised bypass.
* Ramps are negotiated commitments that belong with the other negotiated commitments (Contracts SoR); this gear should *execute* well-formed intents, not *author* schedules.

## Considered Options

1. Portal-side automation (a cron re-keys the transition at the due date).
2. **Pending intents on the aggregate** (`cancelMode`/`resumeAt`; ramp steps) (chosen).
3. A native `SubscriptionSchedule` aggregate that owns multi-step schedules.

## Decision Outcome

Chosen option: **pending intents live on the aggregate.** `cancel` accepts
`cancelMode ∈ {immediate, end_of_term, at(date)}` and `suspend` MAY carry `resumeAt`; a non-immediate
intent is a `ScheduledIntent` row on the aggregate — visible to the renewal job (a pending
end-of-term cancel **suppresses** renewal + next-term recurring), auditable, and **un-schedulable**
until it fires; scheduling and un-scheduling both emit events. At `effectiveAt` the real transition
runs through the full Foundation guard set (Policy/OSS). **Ramps** (SUB-D-04) reuse this mechanism:
Contracts authors the committed schedule, which materialises here as a sequence of scheduled
`changePlan`/`updateQuantity` intents — there is **no native schedule aggregate** at launch.

### Consequences

* The renewal job, Billing, and audit all see the standing intent; a pending cancel can no longer be silently auto-renewed over.
* One scheduling mechanism (`IntentScheduler`, Foundation slice §4.3) serves cancel/resume/ramp — uniform idempotency, eventing, and guard replay.
* Ramps stay Contract-owned (authoring) + Subscriptions-executed (well-formed intents), mirroring the commitment-pool ownership split (rating T-D-14).
* Atomic multi-action submission (Zuora-Orders-style — submit a whole ramp as one order) is **not** covered by this mechanism; it is a Contracts/Design follow-up (SUB-C2, §15).

### Confirmation

* A fixture: a subscription with a pending end-of-term cancel is **not** auto-renewed and emits no next-term recurring; un-scheduling before the term end restores normal renewal; both scheduling and un-scheduling emit events.
* A fixture: a scheduled `changePlan` fires at `effectiveAt` through the full Policy/OSS guard set (a Policy deny at firing time leaves the state unchanged).

## Pros and Cons of the Options

### Portal-side automation (cron re-keys the transition)

* Good: no aggregate change.
* Bad: the intent is invisible to the renewal job/Billing/audit until it fires; auto-renewal races the cancel; no auditable standing-intent record; each portal reinvents it.

### Pending intents on the aggregate (chosen)

* Good: visible to renewal/Billing/audit; one uniform scheduling mechanism; full guard replay at firing; ramps reuse it without a new aggregate.
* Bad: the aggregate carries pending-intent state + its own scheduler singleton; does not by itself give atomic multi-action orders.

### A native SubscriptionSchedule aggregate

* Good: rich multi-step scheduling, atomic orders.
* Bad: duplicates Contracts' role as the home of negotiated commitments; a large new aggregate for a launch that only needs single scheduled intents + Contract-authored ramps; revisit only if self-service ramps become a product goal.

## More Information

Cross-gear seams SUB-C2 (ramps: Contracts authors / Subscriptions executes) and SUB-B2
(`collectionPaused` reuses the same pause mechanism) in [`../SEAMS.md`](../SEAMS.md). Decisions:
[`../DECISIONS.md`](../DECISIONS.md) SUB-D-01, SUB-D-04.

## Traceability

- **PRD**: [`../PRD.md`](../PRD.md) §6.1 (`fr-scheduled-intents`), §6.3 (`fr-ramp-execution`), §6.5 (renewal suppression), §15 (atomic multi-action open).
- **Decisions**: [`../DECISIONS.md`](../DECISIONS.md) SUB-D-01, SUB-D-04.
- **Design**: [`../design/01-foundation-lifecycle.md`](../design/01-foundation-lifecycle.md) §4.3, [`../design/03-plan-changes.md`](../design/03-plan-changes.md) §4.5.
