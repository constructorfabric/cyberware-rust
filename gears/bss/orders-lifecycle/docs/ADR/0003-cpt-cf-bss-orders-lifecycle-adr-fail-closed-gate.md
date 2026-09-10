---
status: accepted
date: 2026-09-09
decision-makers: BSS Orders team
---

# ADR-0003: Fail Closed On An Unevaluable Gate Input

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Fail closed (chosen)](#fail-closed-chosen)
  - [Admit and re-check at activation](#admit-and-re-check-at-activation)
  - [Admit under an explicit tolerated-risk election](#admit-under-an-explicit-tolerated-risk-election)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-orders-lifecycle-adr-fail-closed-gate`

## Context and Problem Statement

The submit gate adopts the published pricing sellability gate **by reference** (PRD §6.1) and adds
the Orders delta. Two of its inputs are not available in the platform as it stands: three of the
six adopted pricing predicates cannot be evaluated from the built read model, and the
against-existing-subscriptions half of the overlap rule needs a presence read (`SUB-O5`) that
Subscriptions does not expose.

An input the gate cannot evaluate is not a rare fault — today it is the normal case. What should
the gate do with it?

## Decision Drivers

* A price-integrity guarantee that admits an unchecked purchase is not a guarantee; the pin is the commit.
* Discovering a sellability failure *after* the first line has provisioned means entering the compensation path, which is the most expensive and least reliable path in the design.
* The same design already admits an unavailable **payment authorization** under an explicit seller election with a recorded risk flag, so "refuse" is not the only shape available and the asymmetry must be deliberate.
* The behaviour is operator-visible: submits will be refused for predicates whose upstream lane is not built yet.
* Whatever is chosen governs every future upstream that is late, not only the two that are late now.

## Considered Options

* **Fail closed** — an unevaluable input is a refusal, with its own machine-readable reason
* **Admit and re-check at activation** — let the submit through and rely on the activation-time market and overlap re-check
* **Admit under an explicit tolerated-risk election** — the shape `05-preconditions` uses for payment authorization: proceed, flag the risk on the order, audit it

## Decision Outcome

Chosen option: **fail closed**. An unevaluable input is a refusal carrying a reason that names
which input was unevaluable, distinct from the reason for a predicate that was evaluated and
failed. `design/03-gate-and-pin.md` **§4.1** is normative for this — it carries the rule ("The gate
**MUST NOT** treat unevaluable as passed under any configuration"); §2.1's design principles and
§2.2's per-port deadlines are about a *slow* port, which is a different condition.

The **tolerated-risk election** was the serious alternative, and it is rejected on the ground that
the two conditions differ in what a tolerating seller is accepting. A tolerated payment
authorization accepts a **credit risk** the seller owns and can price: the buyer may not pay, and
the seller has chosen to provision anyway. An unevaluable sellability predicate accepts an
**unknown**: nobody knows whether the thing being sold may be sold to this party in this market at
this price, and no party is in a position to accept that on the buyer's behalf. A risk flag can
record a decision; it cannot record a fact nobody established.

Admit-and-re-check is rejected because it converts a cheap pre-commit refusal into a post-
provisioning compensation, which is the path the two-phase fulfilment barrier exists to avoid.

### Consequences

* **No submit passes the gate until the missing upstream lanes land.** Three adopted predicates and half the overlap rule are unevaluable, so Phase 1 has no working submit path until `SUB-O5` and the three pricing lanes exist. This is the designed behaviour, not a defect, and `design/README.md`'s phase map must say so.
* Operators see submits refused for predicates that are not yet implemented upstream, with a reason that distinguishes "not evaluable" from "evaluated and failed" — so the refusal is diagnosable rather than mysterious.
* Every future late upstream inherits this posture by default. A capability that wants the other behaviour must argue for a tolerated-risk election explicitly, as payment authorization did.
* The asymmetry with `05-preconditions` is deliberate and recorded here, so a later author does not "fix" one side into consistency with the other.
* The gate's own availability becomes a dependency of order-taking: a pricing outage stops submits rather than degrading them. The per-port deadlines, breakers and bulkheads of `03 §2.2` bound how long that takes to surface, not whether it happens.

### Confirmation

Verified by the reason registry containing exactly one unavailable reason for each of the six
ports — catalog predicates, identity and party, contract resolution, overlap presence, evaluation
and indicative tax — in `design/03-gate-and-pin.md` §3.3; by the gate algorithm collecting
unevaluable inputs into the same all-failures report as evaluated failures (§3.6 *Run Gate and Submit* step 8); and by
a test asserting that an unresolvable port produces a refusal rather than an admission.

## Pros and Cons of the Options

### Fail closed (chosen)

* Good, because an order can never be pinned against a predicate nobody checked.
* Good, because the failure is cheap: nothing has provisioned, and the caller can retry when the lane exists.
* Good, because it needs no new state, no risk column and no policy election.
* Bad, because it makes order-taking unavailable while an upstream is unavailable.
* Bad, because it means the gear ships before it can take an order, which has to be said out loud in the phase map.

### Admit and re-check at activation

* Good, because submits keep working while upstreams are built.
* Good, because the activation re-check already exists for market divergence and overlap collision.
* Bad, because a sellability failure then surfaces after provisioning has begun, in the compensation path.
* Bad, because the order carries a pin composed against unchecked predicates, so the price-integrity NFR is asserted and not held.

### Admit under an explicit tolerated-risk election

* Good, because it matches the shape already used for payment authorization, so the mechanism exists.
* Good, because it puts the choice with the seller, who carries the commercial consequence.
* Bad, because the seller would be accepting an unknown rather than a risk — nothing established whether the sale is permitted.
* Bad, because the flag would record that nobody checked, which is not a decision an audit can act on.

## More Information

Superseded by nothing. The fail-closed posture is stated in `design/01-foundation.md` §2.1
("absence is a refusal") for guard inputs generally; this ADR records the gate-specific decision
and its cost, which the 2026-09-09 review found recorded in no register entry at all.

The operator-visible half is routed to Product as `DECISIONS.md` Q-15.

## Traceability

- **Governing requirements**: `cpt-cf-bss-orders-lifecycle-fr-order-submit`, `cpt-cf-bss-orders-lifecycle-nfr-order-snapshot-integrity`

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-orders-lifecycle-fr-order-submit` — an unevaluable adopted or delta predicate refuses the submit, so the gate admits nothing it did not check
* `cpt-cf-bss-orders-lifecycle-nfr-order-snapshot-integrity` — a pin is never composed against a predicate nobody evaluated, which is what makes the price-integrity claim held rather than asserted
* `cpt-cf-bss-orders-lifecycle-component-gate-and-pin` — the buyer-visible consequence is a refusal naming the unevaluable input, distinct from an evaluated failure, so the case is diagnosable rather than mysterious
* `cpt-cf-bss-orders-lifecycle-component-gate-and-pin` — the decision fixes the gate's posture for every future late upstream, not only the two that are late now
- **PRD**: [`../PRD.md`](../PRD.md) — §6.1 submit gate, §13 dependencies, §16 risks
- **DESIGN**: [`../design/03-gate-and-pin.md`](../design/03-gate-and-pin.md) §2.1, §2.2, §3.3, §4.2; [`../design/01-foundation.md`](../design/01-foundation.md) §2.1
- **Decisions register**: [`../DECISIONS.md`](../DECISIONS.md) — D-72, Q-15
- **Upstream**: [`../UPSTREAM_REQS.md`](../UPSTREAM_REQS.md) — `…-upreq-overlap-presence-read`
