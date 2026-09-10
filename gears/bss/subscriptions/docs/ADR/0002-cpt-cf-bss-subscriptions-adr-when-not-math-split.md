---
status: proposed
date: 2026-07-15
decision-makers: "BSS Subscriptions team"
---

Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

# ADR-0002: Subscriptions Owns the Change Boundary; Rating Owns the Math

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Subscriptions computes proration (self-contained)](#subscriptions-computes-proration-self-contained)
  - [Subscriptions owns only the boundary; rating owns the math (chosen)](#subscriptions-owns-only-the-boundary-rating-owns-the-math-chosen)
  - [A third proration service](#a-third-proration-service)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-subscriptions-adr-when-not-math-split`

> **Status: proposed / flagged for veto.** Records the rationale for the WHEN/MATH split asserted in
> [`../PRD.md`](../PRD.md) §6.3 and seam SUB-R1; ratified when Product/Architecture confirms. Note:
> the sibling pricing gear's working tree previously carried "proration math owned by Subscriptions"
> wording in ~11 places — corrected to this split on 2026-07-15.

## Context and Problem Statement

A plan change, add-on change, or seat change splits a billing period at an instant and prorates the
recurring component across the two sub-intervals. Two questions must have **one owner each**: *when*
does the change take effect (`changeEffectiveAt`, `changeMode`), and *how much* is the prorated
amount (day-count basis, tier-`Q` carry-vs-reset, override resolution, FX). If both live in one gear,
or if the boundary and the math are owned by different gears that each re-derive the other's input,
replay and cross-gear reconciliation drift: the same change can rate differently depending on which
gear's clock or basis is consulted.

Where does the boundary live, and where does the arithmetic live, so that a change replays
byte-identically?

## Decision Drivers

* Determinism: rating replays posted and open periods from a frozen snapshot; the proration math must be a pure function of frozen inputs + **one** authoritative boundary instant.
* Single SoR per fact: the change boundary is subscription lifecycle state; the proration basis (`prorationBasis`, `billingAnchorPolicy`) is catalog-published and rating-evaluated.
* No double math: if Subscriptions and rating both compute proration, the two must agree forever — a standing reconciliation liability.
* Posted-invoice immutability: proration must surface as new/adjusting artifacts (Billing), never an edited posted line — which only works if one gear owns the boundary the artifacts hang off.

## Considered Options

1. Subscriptions computes proration itself (self-contained lifecycle + billing math).
2. Subscriptions owns only the boundary/mode; rating owns all proration math (chosen).
3. Extract a third, shared proration service both gears call.

## Decision Outcome

Chosen option: **Subscriptions owns the WHEN, rating owns the MATH.** Subscriptions sets
`changeEffectiveAt` + `changeMode` (incl. the up/down asymmetry) and emits them on
`SubscriptionPlanChanged` / the composition-changing quantity event; the **rating gear** rates
`planA` over `[periodStart, changeEffectiveAt)` and `planB` over `[changeEffectiveAt, periodEnd)`
(half-open UTC), prorates the recurring component on the frozen `prorationBasis`, applies tier-`Q` /
commitment carry-vs-reset per the snapshot, and slices usage at the **same** boundary. Subscriptions
specifies no day-count math and no override resolution; it fixes only the trigger, the `effectiveFrom`
semantics, and the "no posted-invoice mutation" invariant.

### Consequences

* One boundary owner + one math owner ⇒ a change replays deterministically; there is no second clock or basis to disagree.
* Subscriptions stays free of monetary computation (constraint `no-money`), keeping the lifecycle gear auditable and the math gear the single arithmetic authority.
* Proration always materialises as new billable / credit-debit artifacts keyed off the shared boundary — posted lines are never edited.
* Subscriptions must expose the boundary + composition read model on the shared ordering key `(orderingTenantId, subscriptionId)` — the tenant pinned at creation, immutable across transfers (SUB-D-06); rating consumes it (SUB-R1 counterpart already written in rating PRD §9.2).

### Confirmation

* A joint proration fixture: an immediate mid-cycle upgrade with a posted invoice for the partial period yields a rating-computed adjustment off the Subscriptions boundary, with no edit to the posted line, and replays identically.
* The Subscriptions-emitted `(changeEffectiveAt, changeMode)` and the rating slice boundary are byte-identical in the shared fixtures; Subscriptions contains no day-count code path.

## Pros and Cons of the Options

### Subscriptions computes proration (self-contained)

* Good: one gear to trace for a change.
* Bad: duplicates rating's evaluation engine + `prorationBasis`/`billingAnchorPolicy` frozen state; two arithmetic authorities to reconcile forever; contradicts rating being the math SoR.

### Subscriptions owns only the boundary; rating owns the math (chosen)

* Good: deterministic replay; single arithmetic authority; lifecycle gear stays money-free; posted-immutability holds naturally.
* Bad: requires a tight boundary contract + shared ordering key across two gears (already frozen as SUB-R1).

### A third proration service

* Good: neither gear owns the math.
* Bad: a new deployable + contract for a computation rating already performs; more moving parts, weaker determinism ownership; no product driver.

## More Information

Cross-gear seam SUB-R1 (WHEN/MATH split + shared boundary) and SUB-R3 (seat boundary) in
[`../SEAMS.md`](../SEAMS.md); rating side in [rating PRD](../../../rating/docs/PRD.md) §6.11, §9.2 and
rating design slice `09-period-plan-change`.

## Traceability

- **PRD**: [`../PRD.md`](../PRD.md) §6.3 (`fr-plan-change-boundary`, `fr-proration-ownership`, `fr-proration-triggers`, `fr-update-quantity`), §6.8 (no-retro).
- **Seams**: [`../SEAMS.md`](../SEAMS.md) SUB-R1, SUB-R3.
- **Design**: [`../design/03-plan-changes.md`](../design/03-plan-changes.md) §4.1–§4.2.
