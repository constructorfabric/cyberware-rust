---
status: proposed
date: 2026-07-15
decision-makers: "BSS Subscriptions team"
---

Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

# ADR-0001: Keep the Manifest Status Enum Closed (Trials, Pause, and Intents Are Attributes)

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Add trial / paused / pending statuses to the enum](#add-trial--paused--pending-statuses-to-the-enum)
  - [Model them as attributes, postures, and pending intents on the closed enum (chosen)](#model-them-as-attributes-postures-and-pending-intents-on-the-closed-enum-chosen)
  - [Split the state across two machines (commercial vs billing)](#split-the-state-across-two-machines-commercial-vs-billing)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-subscriptions-adr-manifest-closed-status-machine`

> **Status: proposed / flagged for veto.** Autonomous design decision recording the rationale behind
> [`../DECISIONS.md`](../DECISIONS.md) SUB-D-03 and SUB-D-05; ratified when Product/Architecture
> confirms.

## Context and Problem Statement

The BSS manifest §4.3 fixes `Subscription.status` to exactly **`draft | active | suspended |
cancelled | archived`**. Real commercial lifecycles need more distinctions than five states name
directly: a **trial** period, a **billing-only pause** (service running, collection stopped for
hardship/dispute), a **scheduled cancel** ("cancel at period end"), and enterprise **acceptance**
timing (booking vs service vs customer acceptance). The naïve response is to grow the enum
(`trial`, `paused`, `pending_activation`, …). Every added status multiplies the transition/guard
matrix, breaks manifest conformance, and forces every downstream consumer keying on `status` to
learn the new value fail-closed.

How do we represent trials, billing-only pause, scheduled intents, and acceptance timing **without**
adding a status?

## Decision Drivers

* Manifest conformance: the enum is normative and closed; a new status requires a manifest change **first**, which we do not own on this branch.
* A closed, terminal state machine is what makes lifecycle audit and replay tractable — each added state roughly squares the edge/guard surface.
* Downstream consumers (rating, Billing, OSS, Analytics) key on `status`; an additive status silently breaks a fail-closed consumer until it is taught the value.
* The needed distinctions are **orthogonal to** the commercial state: a trial subscription is genuinely `active`; a collection-paused one is genuinely `active`; a scheduled cancel is genuinely `active` until it fires.

## Considered Options

1. Add `trial` / `paused` / `pending_*` statuses to the enum.
2. Model them as **attributes, postures, and pending intents** on the closed enum (chosen).
3. Split the state across two machines (a commercial machine + a billing machine).

## Decision Outcome

Chosen option: **model every extra distinction as an attribute/posture/pending-intent on the closed
manifest enum.** A **trial** is the leading plan **phase** on a manifest status (`draft` pre-paid-
activation, `active` under trial service) — SUB-D-05's sibling, §6.1 `fr-trials-not-a-status`.
**Billing-only pause** is the `collectionPaused` posture attribute on `active` (SUB-D-03). A
**scheduled cancel/resume** is a `ScheduledIntent` (`cancelMode`/`resumeAt`) pending on the aggregate
(SUB-D-01). The **activation trio** (`contractEffectiveAt`/`serviceActivatedAt`/`customerAcceptedAt`)
are attributes, not `pending_activation`/`pending_acceptance` states (SUB-D-05). Adding a status
value is out of scope until the manifest enum is amended.

### Consequences

* Manifest conformance is preserved; the state machine stays five states with a bounded edge/guard matrix (Foundation slice §4.1).
* Downstream `status` consumers are unaffected by trials/pause/intents — those ride attributes + events the consumers already parse.
* The cost moves to attribute/event modelling: trial phase, `collectionPaused` window, and pending intents each need clear semantics and events (owned by slices 06, 04, 01).
* If a genuine new *state* is ever required, it is a deliberate manifest change — not an ad-hoc gear extension.

### Confirmation

* The `status` domain type is a closed enum in code; a `trial`/`paused`/`pending` value is unrepresentable (compile-time).
* A conformance fixture: a trial subscription, a `collectionPaused` subscription, and one with a pending end-of-term cancel all report a manifest status and never a bespoke value; downstream consumers route them without a new status branch.

## Pros and Cons of the Options

### Add trial / paused / pending statuses to the enum

* Good: each distinction is directly visible in `status`.
* Bad: breaks manifest conformance; squares the guard matrix; forces every consumer to learn each value fail-closed; a manifest change we do not own.

### Model them as attributes, postures, and pending intents on the closed enum (chosen)

* Good: manifest-conformant; bounded state machine; consumers unaffected; audit/replay stay tractable; distinctions are orthogonal to commercial state and belong on attributes.
* Bad: attribute/event semantics must be specified carefully (a posture is not a state); a reader must consult attributes, not `status` alone, to see "on trial" / "collection paused".

### Split the state across two machines (commercial vs billing)

* Good: separates collection concerns from commercial state.
* Bad: two machines to keep consistent; not what the manifest models; over-engineered for a single `collectionPaused` posture.

## More Information

Cross-gear seam analysis and the neighbour impact are in [`../SEAMS.md`](../SEAMS.md) (SUB-D
decisions surface in seams SUB-B2, SUB-C4, SUB-N1). Decisions: [`../DECISIONS.md`](../DECISIONS.md)
SUB-D-01/03/05.

## Traceability

- **PRD**: [`../PRD.md`](../PRD.md) §6.1 (`fr-status-enum`, `fr-trials-not-a-status`, `fr-scheduled-intents`, `fr-activation-instants`), §6.4 (`fr-collection-pause`), §15.
- **Decisions**: [`../DECISIONS.md`](../DECISIONS.md) SUB-D-01, SUB-D-03, SUB-D-05.
- **Design**: [`../design/01-foundation-lifecycle.md`](../design/01-foundation-lifecycle.md) §4.1–§4.4.
