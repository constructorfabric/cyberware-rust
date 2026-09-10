<!-- CONFLUENCE_TITLE: [BSS]: Orders Lifecycle — Buyer Acceptance and the Money Gate (Slice 5) -->
<!-- Related: ../DESIGN.md, ../PRD.md, ./README.md, ./01-foundation.md | Owners: BSS Orders team -->

# DESIGN — Buyer Acceptance and the Money Gate (Slice 5)


<!-- toc -->

- [1. Architecture Overview](#1-architecture-overview)
  - [1.1 Architectural Vision](#11-architectural-vision)
  - [1.2 Architecture Drivers](#12-architecture-drivers)
  - [1.3 Architecture Layers](#13-architecture-layers)
- [2. Principles and Constraints](#2-principles-and-constraints)
  - [2.1 Design Principles](#21-design-principles)
  - [2.2 Constraints](#22-constraints)
- [3. Technical Architecture](#3-technical-architecture)
  - [3.1 Domain Model](#31-domain-model)
  - [3.2 Component Model](#32-component-model)
  - [3.3 API Contracts](#33-api-contracts)
  - [3.4 Internal Dependencies](#34-internal-dependencies)
  - [3.5 External Dependencies](#35-external-dependencies)
  - [3.6 Interactions and Sequences](#36-interactions-and-sequences)
  - [3.7 Database Schemas and Tables](#37-database-schemas-and-tables)
  - [3.8 Deployment Topology](#38-deployment-topology)
- [4. Additional Context](#4-additional-context)
  - [4.1 The acceptance-required election (normative)](#41-the-acceptance-required-election-normative)
  - [4.2 Acceptance on the two paths (normative)](#42-acceptance-on-the-two-paths-normative)
  - [4.3 Authorization as a guard input (normative)](#43-authorization-as-a-guard-input-normative)
  - [4.4 What this design cannot express (normative statement of limitation)](#44-what-this-design-cannot-express-normative-statement-of-limitation)
- [5. Traceability](#5-traceability)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-design-preconditions`

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice owns the two facts that must be true before an order may begin fulfillment: that the
customer agreed to the purchase, and that the payer can pay for it
([`../PRD.md`](../PRD.md) §6.1).

They are different kinds of fact and the design treats them differently. **Acceptance is
recorded**, once, as a first-class instant with its own transition and its own event — because in
a dispute over a partner-placed order it is the only evidence that the customer agreed at all.
**Authorization is consumed**, as a guard input supplied by the sibling gear at the moment of
begin-fulfillment — because it is a point-in-time risk answer owned by a capability that does
not exist yet, and storing it would imply an authority this gear does not have.

The governing rule for acceptance is that it **has no default value at any layer**. The line
model carries an acceptance *due date* with a cascade that fills it from the contract-effective
date, and that cascade must never be mistaken for assent. A due date says when agreement is
expected; the instant says it happened. Conflating them would turn a calendar field into
fabricated consent, which is the one failure mode this slice exists to prevent.

The money gate is deliberately thin, and its thinness is a stated limitation rather than an
omission. Only **provision-then-collect** is expressible: an authorization outcome read after
the order is `approved`, with at-sale money posted downstream when Subscriptions emits billable
facts at activation. A self-service card checkout inverts that ordering, and this design cannot
carry it — §4.4 says so explicitly rather than leaving a reader to discover it.

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|------------------|
| `cpt-cf-bss-orders-lifecycle-fr-order-acceptance` | The acceptance instant is a state-only transition on any non-terminal state, publishing `OrderAcceptanceRecorded`, stored in its own single-row-per-order table with the recording actor and the requirement source. Never defaulted. |
| `cpt-cf-bss-orders-lifecycle-fr-order-payment-auth` | Authorization is a begin-fulfillment guard input, not a stored order fact. Pending and failed are distinct inputs; the seller tolerate-failure election is read at guard time and the resulting risk is flagged and audited. |
| `cpt-cf-bss-orders-lifecycle-fr-order-create` | On the self-service path, submit by the buyer *is* acceptance and is recorded as such in the submit commit — no separate field, no second call. |
| `cpt-cf-bss-orders-lifecycle-fr-orders-boundary-r3-provisioning` | Nothing here provisions or touches money. The slice supplies guard inputs and records one fact. |

#### NFR Allocation

| NFR ID | NFR Summary | Allocated To | Design Response | Verification Approach |
|--------|-------------|--------------|-----------------|----------------------|
| `cpt-cf-bss-orders-lifecycle-nfr-order-audit-completeness` | 100 % of transitions audited | Acceptance recorder | Recording is an engine transition, so the actor, instant and requirement source audit with it | Test asserting an acceptance record always has a paired audit entry naming its actor |
| `cpt-cf-bss-orders-lifecycle-nfr-order-transition-latency` | Commit p95 < 1 s | Acceptance recorder | Recording resolves the requirement source from already-stored data and makes no outbound call | Load test on the acceptance transition |
| `cpt-cf-bss-orders-lifecycle-nfr-order-idempotency` | Zero duplicate effects | Acceptance recorder | At most one acceptance row per order, enforced by the primary key; a replayed recording returns the stored outcome | Concurrency test firing duplicate recordings and asserting one row |

#### Key ADRs

The seven gear ADRs govern this slice. One decision taken here is recorded in the register: **authorization is consumed as a guard
input and never stored as an order fact** ([`../DECISIONS.md`](../DECISIONS.md) D-78, §4.3),
whose alternative was a `payment_pending` order state.

### 1.3 Architecture Layers

Inherited from [`01-foundation`](./01-foundation.md) §1.3. The authorization port is owned by
the sibling gear, so this slice adds no infrastructure of its own.

## 2. Principles and Constraints

### 2.1 Design Principles

#### The acceptance instant is never defaulted

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-acceptance-never-defaulted`

No policy, cascade, migration or convenience path may write an acceptance instant that a party
did not perform. The line-level acceptance **due date** has a cascade; the instant has none.
Because the column exists only when a real instant was recorded, its presence is evidence and
its absence is a truthful answer.

#### Agreement and delegation are different facts

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-agreement-not-delegation`

On the partner-placed path the order evidences the partner's **right to act** — carried by the
initiating actor and the delegation proof the engine's authorization pre-guard demands. It does
not evidence the customer's **agreement to the purchase**. The two are recorded separately
because a dispute distinguishes them, and a design that conflated them would offer a partner's
own authority as proof of their customer's consent.

#### Authorization is read, not owned

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-principle-authorization-read-not-owned`

The authorization outcome is a point-in-time answer from a capability this platform has not
specified. This slice consumes it as a guard input at begin-fulfillment and stores no
instrument, no token and no outcome as an order fact. Storing it would imply this gear could
answer "is the payer good" later, which it cannot.

### 2.2 Constraints

#### There is no `payment_pending` state

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-no-payment-pending-state`

A pending authorization leaves the order `approved` with begin-fulfillment simply not called. No
twelfth state is added. The consequence is real and is not hidden: an order awaiting
authorization and a healthy order awaiting the sibling gear's next step are indistinguishable
from the order document alone, and process visibility is the sibling gear's to provide.

#### A declined instrument exits by expiry, and only where the TTL is set

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-declined-instrument-exit`

Where authorization fails and the seller has not elected tolerate-failure, begin-fulfillment is
withheld and the order remains `approved` until its TTL elapses. Two qualifications, and both are
this constraint's real content rather than footnotes. The `approved` TTL is a **Product-owned open
question with no code default**, so **while it is unset this order has no automatic exit at all** —
only a caller-driven cancel retires it ([`07-hold-and-expiry`](./07-hold-and-expiry.md) §4.5). And
where it *is* set, the **resume cap** of `07 §4.2` is what stops a hold/resume cycle restarting the
dwell without limit, bounding the exit at `(cap + 1) × TTL` rather than at nothing
(`../DECISIONS.md` D-90). An earlier version of this constraint claimed an absolute order lifetime
retired the order in the unset case; that backstop is withdrawn and `07 §4.2` states why, so the
unset case is disclosed here rather than covered. There is no re-authorize
operation, no payment failure event and no order-visible outcome, because there is no Payments
capability to supply one. This is routed as [`../DECISIONS.md`](../DECISIONS.md) Q-08 — the PRD
carries **no** §15 row for it — and stated as a designed limitation rather
than deferred silently: the order expires, and the buyer learns nothing from the order document.

#### Payment collection is out of scope entirely

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-constraint-no-payment-collection`

Capture, settlement, strong-customer-authentication challenges and their asynchronous return,
retry with an alternative instrument, refunds, chargebacks and provider webhooks are all outside
this gear and outside the sibling gear. The slice consumes an **authorization** outcome and
nothing further. No cardholder data is handled anywhere, so payment-card compliance is not
applicable to this gear.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-acceptance-record`

The customer-acceptance instant as a recorded fact: when it happened, which actor recorded it,
and whether the requirement came from a referenced contract or from the platform default. At
most one per order. Its presence gates begin-fulfillment where acceptance is required; its
absence is never inferred to be satisfaction.

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-entity-authorization-outcome`

A **transient** guard input, not a persisted entity: the authorization verdict — authorized,
pending or failed — supplied by the sibling gear at begin-fulfillment. Its only durable trace is
the risk flag and the audit entry written when a tolerate-failure election admits a failed
authorization.

**Relationships**:
- `Order root` → `Acceptance record`: zero-or-one. Present only where a real instant was recorded.
- `Acceptance record` → `Order transition`: one-to-one; the recording is itself an audited transition.
- `Authorization outcome` → `Order transition`: contributes to the begin-fulfillment audit entry, including the risk flag where one applies.

### 3.2 Component Model

This slice realises `cpt-cf-bss-orders-lifecycle-component-preconditions`
([`../DESIGN.md`](../DESIGN.md) §3.2) as two internal parts.

#### Acceptance recorder

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-component-preconditions-acceptance`

##### Why this component exists

Without a recorded instant there is nothing showing the customer agreed to a partner-placed
purchase, and the order's whole evidentiary value collapses at the point it matters most.

##### Responsibility scope

Resolution of the acceptance-required election from the referenced contract or the platform
default; recording the instant with its actor; the self-service rule that makes buyer submit
constitute acceptance; and the at-most-one-per-order invariant.

##### Responsibility boundaries

It writes no default value under any policy. It does not decide whether fulfillment may begin —
it supplies one of two guard inputs.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-transition-orchestrator` — depends on
- `cpt-cf-bss-orders-lifecycle-component-preconditions-money-gate` — shares model with

#### Money gate

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-component-preconditions-money-gate`

##### Why this component exists

Without a money check before provisioning, a non-paying tenant receives resources and the
failure surfaces later as dunning over consumed capacity — the most expensive compensation path
the platform has.

##### Responsibility scope

The begin-fulfillment guard: the authorization outcome as input, the three-way distinction
between authorized, pending and failed, the seller tolerate-failure election read at guard time,
and the risk flag recorded when a failed authorization is tolerated.

##### Responsibility boundaries

It holds no payment mechanism, no instrument and no token, performs no credit scoring, and
stores no authorization outcome as an order fact.

##### Related components (by ID)

- `cpt-cf-bss-orders-lifecycle-component-workflow-seam` — shares model with

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-interface-preconditions-ops`

- **Requirement**: `cpt-cf-bss-orders-lifecycle-interface-order-ops`
- **Technology**: REST/OpenAPI via `OperationBuilder`; RFC 9457 problems

| Method | Path | Description | Stability |
|--------|------|-------------|-----------|
| `POST` | `/bss-orders-lifecycle/v1/orders/{orderId}/acceptance` | Record the customer-acceptance instant | unstable |
| `GET` | `/bss-orders-lifecycle/v1/orders/{orderId}/acceptance` | Read the recorded instant, or its absence | unstable |

The begin-fulfillment operation itself is owned by
[`06-workflow-seam`](./06-workflow-seam.md); this slice contributes its two guards.

**Reasons contributed to the registry**: acceptance-already-recorded,
acceptance-not-required-for-order, acceptance-required-not-recorded,
authorization-pending, authorization-failed, authorization-failed-tolerated (an admission
carrying a risk flag, not a refusal), and **acceptance-requirement-unevaluable** — the
unevaluable reason for this slice's guard inputs, required because `01 §3.6` *Attempt Transition* step 3.1 settles an
unresolvable input with "the guard's registered unevaluable reason" and this slice resolves the
acceptance requirement from the contracts port, whose unreachability would otherwise have no
registered name. A terminal order needs no slice reason: transition row 25
admits the acceptance instant **from any non-terminal state**, so a terminal order has no row and
the engine's own `not-admissible` refuses it — matching the treatment
[`07-hold-and-expiry`](./07-hold-and-expiry.md) §3.3 gives an ineligible expiry
([`../DECISIONS.md`](../DECISIONS.md) D-38).

### 3.4 Internal Dependencies

| Dependency Gear | Interface Used | Purpose |
|-------------------|----------------|----------|
| `toolkit-db` | Runtime-scoped access, via the engine | The acceptance record, inside the transition transaction |

### 3.5 External Dependencies

| Dependency Gear | Interface Used | Purpose |
|-------------------|---------------|---------|
| `contracts` | SDK client, via the gate slice's port | The acceptance-required election where a contract is referenced; platform default otherwise |
| Payments | Reached only through `orders-workflow` | The authorization outcome. No gear and no specification exists in this repository; the outcome arrives as a guard input |

**Dependency Rules** (per project conventions):
- No circular dependencies
- Always use SDK modules for inter-gear communication
- No cross-category sideways deps except through contracts
- Only integration/adapter gears talk to external systems
- `SecurityContext` must be propagated across all in-process calls

### 3.6 Interactions and Sequences

#### Record the acceptance instant

**ID**: `cpt-cf-bss-orders-lifecycle-seq-record-acceptance`

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-new-acquisition`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-partner-admin`, `cpt-cf-bss-orders-lifecycle-actor-orders-direct-customer`

**Algorithm: Record Acceptance**

Input: order_id, recording_actor, security_context, idempotency_key
Output: recorded, or a registered refusal

1. [ ] - `p1` - Declare four guards the engine evaluates and audits: non-terminal, **recording-path-admissible**, not-already-recorded and recording-party - `inst-ra-declare-guards`
2. [ ] - `p1` - Resolve whether acceptance is required: from the referenced contract where present, else the platform default - `inst-ra-resolve-requirement`
3. [ ] - `p1` - Resolve the recording-path-admissible input and the requirement source together: on the **self-service** path a separate recording is inadmissible where acceptance is not required, because the instant rode the submit commit, and the guard refuses with `acceptance-not-required-for-order`; on the **partner-placed** path the recording is admissible either way, with `requirement_source` resolving to `contract`, `platform-default` or `volunteered` (§4.1) - `inst-ra-resolve-path-admissibility`
4. [ ] - `p1` - Resolve the not-already-recorded guard's input by reading whether an acceptance row exists; a second attempt refuses with `acceptance-already-recorded` - `inst-ra-resolve-already-recorded`
5. [ ] - `p1` - Record the instant as the commit time, with the recording actor and the requirement source - `inst-ra-record-instant`
6. [ ] - `p1` - Request the state-only acceptance transition so the engine audits it and publishes OrderAcceptanceRecorded - `inst-ra-request-transition`
7. [ ] - `p1` - **RETURN** recorded - `inst-ra-return-recorded`

**Description**: The instant is the commit time, not a caller-supplied value, because a
caller-supplied instant is a claim rather than an observation. The event is what lets the
sibling gear re-evaluate begin-fulfillment eligibility without polling.

#### Self-service submit constitutes acceptance

**ID**: `cpt-cf-bss-orders-lifecycle-seq-self-service-acceptance`

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-new-acquisition`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-direct-customer`

```mermaid
sequenceDiagram
    participant C as Direct Customer
    participant L as Orders Lifecycle
    C ->> L: submit (buyer is the resource tenant)
    L ->> L: gate passes; record acceptance in the same commit
    L -->> C: submitted, acceptance recorded
    Note over L: one commit, two facts, ONE event -<br/>OrderSubmitted carries the acceptance instant
```

**Description**: On the self-service path the submitting party *is* the accepting party, so a
second call would ask the buyer to agree to something they just bought. The recording is a
contribution to the submit commit and audits with it, and the instant travels in the
`OrderSubmitted` payload — a single event, because a single transition emits a single event.

#### Begin-fulfillment guard evaluation

**ID**: `cpt-cf-bss-orders-lifecycle-seq-begin-fulfillment-guards`

**Use cases**: `cpt-cf-bss-orders-lifecycle-usecase-order-fulfillment-complete`

**Actors**: `cpt-cf-bss-orders-lifecycle-actor-orders-workflow`, `cpt-cf-bss-orders-lifecycle-actor-orders-contracts`

**Algorithm: Evaluate Begin-Fulfillment Preconditions**

Input: order_id, authorization_outcome, security_context
Output: admit, admit-with-risk-flag, or a registered refusal

1. [ ] - `p1` - Resolve whether acceptance is required for this order - `inst-bg-resolve-requirement`
2. [ ] - `p1` - **IF** required **AND** no acceptance row exists: - `inst-bg-if-acceptance-missing`
   1. [ ] - `p1` - **RETURN** acceptance-required-not-recorded refusal - `inst-bg-return-acceptance-missing`
3. [ ] - `p1` - **MATCH** the authorization outcome: - `inst-bg-match-authorization`
   1. [ ] - `p1` - **WHEN** authorized: **RETURN** admit - `inst-bg-when-authorized`
   2. [ ] - `p1` - **WHEN** pending: **RETURN** authorization-pending refusal (the order stays `approved`) - `inst-bg-when-pending`
   3. [ ] - `p1` - **WHEN** failed: - `inst-bg-when-failed`
      1. [ ] - `p1` - Read the seller tolerate-failure election - `inst-bg-read-tolerate-election`
      2. [ ] - `p1` - **IF** tolerate-failure is not elected: **RETURN** authorization-failed refusal - `inst-bg-if-not-tolerated`
      3. [ ] - `p1` - **RETURN** admit-with-risk-flag, to be recorded on the order and audited - `inst-bg-return-tolerated`

**Description**: Pending and failed are genuinely different answers and collapsing them would
either stall silently or provision a non-paying tenant. Neither refusal changes order state —
the order stays `approved`, and the sibling gear retries or escalates.

### 3.7 Database Schemas and Tables

This slice owns `orders_acceptance` (`cpt-cf-bss-orders-lifecycle-dbtable-acceptance`),
specified normatively in [`01-foundation`](./01-foundation.md) §3.7, and introduces
`orders_policy_election` below. Three additions to the engine-owned tables belong here:

- The primary key is `order_id`, which is what enforces **at most one acceptance per order** structurally rather than by check.
- `accepted_at` is **NOT NULL with no default at any layer** — no column default, no application default, no backfill. The row exists only because an instant was recorded.
- `requirement_source` records whether the requirement came from a contract or the platform default, so a later change to the default does not rewrite history about why acceptance was demanded.

The **risk flag** for a tolerated authorization failure is
`orders_order.authorization_failure_tolerated_at`, specified in
[`01-foundation`](./01-foundation.md) §3.7, written by the begin-fulfillment transition and never
cleared, since it records a decision taken at a moment rather than a current condition.

#### Table: orders_policy_election

**ID**: `cpt-cf-bss-orders-lifecycle-dbtable-policy-election`

**Schema**: `election` enum (`tolerate_authorization_failure`, `acceptance_required`), `scope`
enum (`platform`, `seller`), `scope_id` (null for platform scope), `elected`, `elected_by`,
`elected_at`.

**PK**: (election, scope, scope_id)

**Constraints**: `NULLS NOT DISTINCT` on the key so a second platform row for one election is
impossible; `scope_id` NOT NULL where `scope = 'seller'` and NULL where `scope = 'platform'`.
Mutable — an election is a standing policy that a seller may change.

**Additional info**: seller scope overrides platform scope. An election with no row is read as its
safe value (tolerate-failure not elected; acceptance required), and the guard records whether it
read an explicit row or the fallback, so an audit can distinguish a decision from an omission.
This is the only table this slice introduces; the delivery path is the same policy channel that
carries `orders_state_ttl_policy` ([`../DESIGN.md`](../DESIGN.md) §3.8).

### 3.8 Deployment Topology

Inherited from [`01-foundation`](./01-foundation.md) §3.8. No background worker.

**Observability owned here**: authorization-outcome distribution across authorized, pending and
failed, kept as three series because collapsing them hides the declined-instrument population
§4.4 says has no exit but expiry; the count of admissions carrying the **tolerated-failure risk
flag**, which is a commercial risk register and not an error rate; acceptance-recording latency
against the acceptance-due date; and recording-party refusals, since those are the control
preventing a placing party supplying its own customer's consent. Alerts fire on the
tolerated-failure count crossing its threshold, on any sustained recording-party refusal rate
(a security signal), and on the authorization port reporting unavailable, which fails begin
fulfillment closed.

## 4. Additional Context

### 4.1 The acceptance-required election (normative)

Whether acceptance is required **MUST** be sourced from the referenced contract where one
exists, and from the platform default otherwise. The resolved source **MUST** be stored on the
acceptance row, so a later change to the platform default does not retroactively change the
recorded reason.

Where acceptance is required, the order **MUST NOT** enter `in_fulfillment` until the instant is
recorded. Where it is **not** required, a recording attempt on the **partner-placed** path
**MUST** still be admitted, and `requirement_source` **MUST** record it as `volunteered` rather
than `contract` or `platform_default` — so the row is never mistaken for evidence that policy
demanded it.

The previous rule refused the recording outright, on the reasoning that an acceptance row on an
order that never needed one is misleading evidence. That reasoning protects evidentiary hygiene
and defeats the requirement it serves: PRD §6.1's "a customer-acceptance instant **MUST** be
recordable as a first-class fact" is unconditional, and the required flag governs only whether
fulfilment waits for it. On an uncontracted or platform-default partner-placed order — the common
case — a genuine customer agreement could not be recorded at all, which is precisely the dispute
scenario the requirement exists for. Provenance on the row solves the hygiene problem without
withholding the fact ([`../DECISIONS.md`](../DECISIONS.md) D-71).

### 4.2 Acceptance on the two paths (normative)

On the **self-service** path, submit by the buyer **constitutes** acceptance and **MUST** be
recorded as such within the submit commit — as a **contribution to the submit transition**, not as
a second transition. Only `OrderSubmitted` is published, carrying the acceptance instant in its
payload; `OrderAcceptanceRecorded` is published **only** on the partner-placed path. One commit
cannot be two transition rows and cannot enqueue two outbox rows without breaking the engine's
one-row invariant and the outbox's per-order sequence uniqueness
([`../DECISIONS.md`](../DECISIONS.md) D-16). There is no separate field and no second call.

On the **partner-placed** path, acceptance **MUST** be recorded as a separate first-class
instant, publishing `OrderAcceptanceRecorded`. The instant **MUST** be the commit time rather
than a caller-supplied value.

**Only a principal of the `resourceTenantId` party may record a partner-placed order's acceptance
instant.** The actor who created or submitted the order is therefore barred, and a Seller Operator
has no acceptance-recording authority. This prevents the commercially interested placing or
selling party supplying the customer's consent without a specified, verifiable authority artifact
([`../DECISIONS.md`](../DECISIONS.md) D-31). The permission declaration in
[`08-read-and-authz`](./08-read-and-authz.md) §4.3 carries the rule; this slice carries the guard
that enforces it.

The line-level **acceptance due date is a calendar field** and **MUST NOT** satisfy the instant
under any circumstance. The cascade in [`02-capture`](./02-capture.md) §4.2 that fills it from
the contract-effective date **MUST NOT** be read as defaulting the instant. This is stated twice
across two slices deliberately, because it is the one place where a convenience default would
manufacture consent.

### 4.3 Authorization as a guard input (normative)

The payment-authorization outcome **MUST** be consumed as a begin-fulfillment guard input
supplied by the sibling gear, and **MUST NOT** be stored as an order fact. **Pending** and
**failed** are distinct outcomes and **MUST NOT** be collapsed: pending leaves the order
`approved` with begin-fulfillment uncalled, and failed does the same unless the seller has
elected tolerate-failure.

Where tolerate-failure is elected and authorization failed, begin-fulfillment **MAY** proceed and
the risk **MUST** be flagged on the order and audited. The flag records a decision taken at an
instant and **MUST NOT** be cleared later.

**Where the election is stored.** Both begin-fulfillment policy inputs are **policy rows, not
code defaults**, held in `orders_policy_election` (this slice, §3.7) keyed
`(election, scope, scope_id)` with `scope` in (`platform`, `seller`) and seller scope overriding
platform. The two elections are `tolerate_authorization_failure` and `acceptance_required`. An
**unset** election is read as its safe value — tolerate-failure **not** elected, acceptance
**required** — and the read is recorded so an audit can tell an explicit election from a fallback.
This matters because the design previously specified both as reads with no source: no table, no
key, no scope, no default and no delivery path, so an implementer would have invented a default —
which §2.1 forbids outright for acceptance ([`../DECISIONS.md`](../DECISIONS.md) D-66).

No order state is introduced for any authorization outcome. The rejected alternative was a twelfth,
`payment_pending` state; it was rejected because it would need its own TTL, its own guards, its
own event and its own place in the transition table, all to represent a condition that is
external, transient and already visible in the sibling gear's process state. **Credit scoring is
out of scope and stays out of scope.**

### 4.4 What this design cannot express (normative statement of limitation)

Only **provision-then-collect** is expressible: an authorization read after `approved`, with
at-sale money posted downstream when Subscriptions emits billable facts at activation. A
self-service card checkout **inverts** this — the buyer is charged at checkout and expects
service only if the charge succeeds — and the difference is structural rather than a tuning
parameter, because capture, strong-customer-authentication challenges, retry with an alternative
instrument, and refund-as-reversal have no owning capability.

Two consequences follow and are recorded rather than discovered. A **declined instrument** leaves
the order `approved` with no payment event, no re-authorize operation and expiry as its only
exit, indistinguishable from a healthy order awaiting its next process step. And a **reversal
after capture** would be a refund through a payment provider rather than a Billing credit note,
which is a different artifact with a different system of record than the compensation path
assumes.

Neither is a PRD open question: PRD §15's fifteen rows carry nothing about payment ordering, a
declined instrument's exit, or refund-as-reversal. Both are routed in this gear's own register as
[`../DECISIONS.md`](../DECISIONS.md) Q-08, owned by Architecture with Product, and a PRD amendment
adding the row is the ask. Closing either requires a
Payments capability specification that does not exist in this repository, and this design
**MUST NOT** invent one.

## 5. Traceability

- **PRD**: [`../PRD.md`](../PRD.md) — §6.1 buyer acceptance and payment-authorization precondition
- **Gear design**: [`../DESIGN.md`](../DESIGN.md) — realises `cpt-cf-bss-orders-lifecycle-component-preconditions`
- **Engine**: [`01-foundation`](./01-foundation.md) — acceptance table, transition contract, audit
- **Depends on**: [`02-capture`](./02-capture.md) for the acceptance-due-date cascade this slice must not be confused with; [`03-gate-and-pin`](./03-gate-and-pin.md) because the self-service acceptance instant is a contribution to the submit transition that slice owns
- **Consumers**: [`06-workflow-seam`](./06-workflow-seam.md) composes both guards into begin-fulfillment; [`07-hold-and-expiry`](./07-hold-and-expiry.md) owns the TTL that is a declined instrument's only exit
- **ADRs**: [`ADR/0001`](../ADR/0001-cpt-cf-bss-orders-lifecycle-adr-transition-through-engine.md) transition through the engine; [`ADR/0002`](../ADR/0002-cpt-cf-bss-orders-lifecycle-adr-slice-decomposition.md) the foundation-plus-seven-slices decomposition