<!-- CONFLUENCE_TITLE: [BSS]: Orders Lifecycle — Upstream Requirements -->
<!-- Related: ./DESIGN.md, ./DECISIONS.md, ./design/ | Owners: BSS Orders team -->

# UPSTREAM_REQS — Orders Lifecycle

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Requesting Gears](#12-requesting-gears)
- [2. Requirements](#2-requirements)
  - [2.1 Subscriptions](#21-subscriptions)
  - [2.2 Rating / price evaluation](#22-rating--price-evaluation)
  - [2.3 Billing chain](#23-billing-chain)
  - [2.4 Account Management](#24-account-management)
  - [2.5 Payments](#25-payments)
  - [2.6 Orders Workflow](#26-orders-workflow)
- [3. Priorities](#3-priorities)
- [4. Traceability](#4-traceability)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

What this gear needs from gears it does not own, declared here so a future specification of those
gears is authored with these obligations visible. Until now these asks lived only in slice prose,
which is how the `SUB-O*` numbering forked between the Subscriptions seam map and the sibling
Orders Workflow PRD ([`DECISIONS.md`](./DECISIONS.md) Q-04).

### 1.2 Requesting Gears

| Requesting gear | Why it needs the target |
|-----------------|-------------------------|
| `orders-lifecycle` | Owns the order document and its state machine; needs Products to expose a version-pinned `sellable` read for the sixth adopted gate predicate. Needs Subscriptions to accept an explicit start instant, expose an overlap-presence read, and carry an order reference and a compensation cancellation reason. Needs Rating to supply a pre-subscription evaluation and an annualised TCV figure. Needs the billing chain to propagate the external reference and to answer an indicative tax read. Needs Account Management to issue verifiable delegation proof. Needs a Payments capability that does not exist. |
| `orders-workflow` | Must consume `OrderAmended`, obtain the approval-requirement verdict for the new order version, and reflect the new version onward from `submitted`; without this the Lifecycle two-step re-approval seam stalls. |

## 2. Requirements

### 2.1 Subscriptions

The seam-map numbering (`SUB-O1`…`SUB-O6`) is treated as canonical here, per Q-04. Of the
identifiers the Workflow PRD adds beyond the seam map, **only `SUB-O9`** (correlation
propagation) is an ask of this gear; it is carried below and flagged as unregistered upstream,
since the seam map does not define it. **`SUB-O4`, `SUB-O6`, `SUB-O7` and `SUB-O8`
are not asks of this gear** and are deliberately absent from this register — `SUB-O6` notably so,
since Q-04 records it carrying two different meanings across the two registers. `SUB-O3` is
registered below but is a **preservation** ask rather than a change request, which is why the
slices that rest on it do not count it among their unagreed dependencies.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-subscription-start-instant`

The activation intent and `create` **MUST** accept an explicit **start instant** and **MUST NOT**
derive the subscription start from any date carried on the order. Where the two-phase activation
barrier defers a line past its quoted service-activation date, the spawned subscription's start is
the **actual activation instant**; billing and entitlement **MUST NOT** be backdated to the
earlier quoted date. Raised as **`SUB-O10`** — a new ask, not present in either existing register.
Without it the PRD's no-backdating requirement is unenforceable from the order side, because
Subscriptions owns the start. See [`DECISIONS.md`](./DECISIONS.md) D-56.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-overlap-presence-read`

An **overlap-key presence read**: given an overlap scope key and a payer, does a non-terminal
subscription already hold that key, and what is the effective concurrent-active cardinality.
Registered upstream as **`SUB-O5`**, unagreed. The against-existing-subscriptions half of the
submit gate's overlap predicate depends on it; until it lands that half is unevaluable and
therefore a refusal, which fails closed.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-compensation-cancel-reason`

A cancellation **reason value for order-fulfillment compensation**, scoped **out** of the
early-termination class so it derives neither a termination fee nor a credit. Registered upstream
as **`SUB-O1`**, marked critical there, unagreed. The upstream note records that reason values
ride event payloads consumers key on, so adding one after Billing consumes the contract is a
breaking change — making this the ask materially cheaper now than later.

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-order-reference-on-create`

An optional **order reference** (`orderId` plus order-line reference) accepted on `create`, so
"which order produced this subscription" is answerable from the subscription side. Registered
upstream as **`SUB-O2`**, unagreed. This gear already persists the forward mapping; without the
reverse, provenance is one-directional and a subscription created outside the order path is
indistinguishable from one created through it.

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-two-phase-pair-preserved`

The `draft → activate` pair **MUST** remain externally callable, with the `draft → cancelled` void
remaining not resource-affecting. Registered upstream as **`SUB-O3`**. No change is requested —
only that the contract already established is not collapsed into a create-and-activate
convenience, because order-level atomicity is built entirely on it.

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-correlation-propagation`

The process **correlation identifier** echoed on confirmations and propagated toward the Policy
Engine and OSS. Cited by the sibling Workflow PRD as `SUB-O9`; **not present in the seam map**, so
unregistered upstream. Without it an end-to-end acquisition trace stops at the seam.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-overlap-activation-atomicity`

**Atomic enforcement of `overlapScopeKey` at the point a subscription commits to `active`.** The
order axis of the overlap rule is closed inside this gear's transition transaction by
`01 §3.7`'s partial unique index, but the **subscription** axis cannot be: the committing
transaction belongs to Subscriptions, so no re-check performed here can be atomic with it. Two
activation waves can therefore each pass this gear's re-check and jointly exceed
`maxConcurrentActive`. Subscriptions **MUST** re-evaluate the key and commit `active` under one
reservation or serialisation boundary. Until it does, **the gap is open and `03 §2.2` does not
bound it** — that section states why no timed validity window is assertable from this side, since
nothing this design declares carries a deadline to the party that would have to honour it, and
`spawn-signal` is event-less. What holds meanwhile is narrower: the re-check is an **early abort**
with no admission guarantee, and a collision appearing at or after activation surfaces as
`overlap-collision` on the failure-acknowledgement path with compensation evidence
(`DECISIONS.md` D-89). This is the same seam as `SUB-O5`, which supplies the *read*; this ask is
the *enforcement*, and the read alone does not make the rule hold.

### 2.2 Rating / price evaluation

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-pre-subscription-evaluation`

A **pre-subscription resolved-total evaluation** operation: given the order's scope inputs — plan,
price and SKU references, quantity, currency, region, contract reference and term — produce the
resolved total for price-list scopes that need subscription-level context. PRD §13 names this
**required** for the `p1` price-evaluation dependency, and PRD §15 row 6 is the open question it
closes. Until it exists, overlays requiring subscription-level context are **excluded** from the
order-time total and the exclusion is stated on the read and Preview responses
([`./design/03-gate-and-pin.md`](./design/03-gate-and-pin.md) §4.5) — a truthful interim, but one
that becomes permanent by default while no ask is registered.

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-tcv-with-annualisation`

The **net pre-tax total-contract-value figure**, computed by the evaluation domain and carrying the
PRD's semantics: usage excluded, one-time charges counted once, and an open-ended term annualised
at the line's billing cycle (12 monthly, 4 quarterly, 1 annual — PRD §12 AC-2d). D-40 moved this
computation off this gear to satisfy R4's prohibition on price arithmetic, which places the
obligation here; without the ask, TCV for every rolling deal is undefined and two rolling deals
differing only in billing cycle become incomparable at the approval gate — the exact comparison
the annualisation exists to enable.

### 2.3 Billing chain

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-external-reference-propagation`

The **external reference** carried onto billing documents. PRD §13 makes it a `MUST` — "External
reference on the order/line **MUST** propagate to billing documents" — for buyer-side
accounts-payable reconciliation. This gear publishes it on `OrderSubmitted` and `OrderCompleted`,
but billing documents derive from the **subscription**, not from order events, so the hop past the
event payload is unspecified. Either the activation intent must accept it and Subscriptions carry
it onto the billable facts, or Billing must consume it from the order events directly and say so.
A purchase-order number that never reaches the invoice fails the requirement invisibly: the order
shows it, the event carries it, and only the invoice lacks it.

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-indicative-tax-read`

An **indicative tax read** — per line and in total for a basket — from the billing-chain tax
owner, which PRD §9.1 requires Preview to return and this design never stores. No gear or
specification for a tax owner exists in this repository, so this ask has no target to register
against and is recorded here for whichever specification takes it.

### 2.4 Account Management

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-delegation-proof-credential`

A **verifiable delegation-proof credential**: a signed assertion naming the delegating tenant, the
delegated scope, the delegate, an issue instant and a finite expiry; verifiable against a published
issuer key; revocable by the delegating tenant with revocation observable at verification time.
Aligned with BSS manifest §2.1.3. This is the single control preventing cross-tenant leakage on the
partner-placed path, and it has no specified form today. See [`DECISIONS.md`](./DECISIONS.md) D-32.

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-payer-commercial-profile`

A read of the **payer's commercial profile** yielding the `(currency, region)` binding the order
market is derived from, and a party-eligibility answer where a contract is referenced. Consumed at
submit and re-read before the first activation intent.

### 2.5 Payments

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-authorization-outcome`

An **authorization outcome** for a payer, distinguishing **authorized**, **pending** and **failed**
as three separate answers. Consumed as a begin-fulfillment guard input and never stored as an order
fact. **No Payments capability and no specification exists in this repository**, so this ask has no
target gear to register against; it is recorded here so that a future Payments specification is
authored with it visible. Capture, settlement, strong-customer-authentication and refunds are
explicitly **not** requested — see [`DECISIONS.md`](./DECISIONS.md) Q-08 for the consequence.

### 2.6 Orders Workflow

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-workflow-amendment-verdict`

On consuming `OrderAmended`, Orders Workflow **MUST** terminate or supersede prior-version
processing, obtain the approval-requirement verdict keyed by the event's `orderId` and new
`orderVersion`, and reflect that verdict into Orders Lifecycle: `submitted → pending_approval` for
approval required, or `submitted → approved` where it is not required. It **MUST NOT** carry the
prior version's verdict forward. This is required by the Lifecycle two-step amendment seam; until
the reflection arrives, the order remains `submitted` and its submitted TTL continues to run.

The current Workflow PRD restricts verdict acquisition to `OrderSubmitted`; it therefore needs an
amendment before this seam can be implemented. See `DECISIONS.md` Q-12.

### 2.7 Products

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-lifecycle-upreq-sellable-flag-read`

A **version-pinned read of the registry `sellable` flag** for a SKU or plan, resolvable at a
`catalogVersion` the caller fixes rather than at "now", and answerable for a whole basket in one
call. It is adopted predicate **(6)** of the submit gate, and it is **not a pricing fact**: the
`pricing` gear publishes the adopted predicate set, but the flag is owned by `products`, which
carries a PRD and no implementation. Until it exists the predicate is **unevaluable** and every
submit that reaches it fails closed ([`./ADR/0003-cpt-cf-bss-orders-lifecycle-adr-fail-closed-gate.md`](./ADR/0003-cpt-cf-bss-orders-lifecycle-adr-fail-closed-gate.md)),
which makes `products` a submit-path dependency of the **same standing as pricing** — the point
[`./design/README.md`](./design/README.md) makes about the phase map, and the reason the ask is
registered here rather than left in slice prose. The version pin is part of the ask, not an
optimisation: a flag read at "now" would let the frontier advance mid-run and admit a basket under
two different catalog versions ([`./design/03-gate-and-pin.md`](./design/03-gate-and-pin.md) §3.6
step 3).

## 3. Priorities

| Priority | Requirements |
|----------|-------------|
| `p1` (critical) | `…-upreq-subscription-start-instant`, `…-upreq-overlap-presence-read`, `…-upreq-overlap-activation-atomicity`, `…-upreq-compensation-cancel-reason`, `…-upreq-pre-subscription-evaluation`, `…-upreq-tcv-with-annualisation`, `…-upreq-external-reference-propagation`, `…-upreq-delegation-proof-credential`, `…-upreq-authorization-outcome`, `…-upreq-workflow-amendment-verdict`, `…-upreq-sellable-flag-read` |
| `p2` (important) | `…-upreq-order-reference-on-create`, `…-upreq-two-phase-pair-preserved`, `…-upreq-correlation-propagation`, `…-upreq-indicative-tax-read`, `…-upreq-payer-commercial-profile` |

`…-upreq-overlap-activation-atomicity` and `…-upreq-overlap-presence-read` are tracked
**separately** and both are outstanding: the presence read supplies the *read* the gate's
pre-check needs, the activation atomicity supplies the *enforcement* on the subscription axis, and
neither substitutes for the other (§2.1). The register above is the authoritative list of `p1`
blockers, so an ask marked `p1` in §2 that is absent from it understates what is outstanding.

Two asks are cheaper now than later for structural reasons rather than scheduling ones. The
compensation cancel reason rides event payloads that downstream consumers key on. The start
instant determines whether a deferred line's subscription can ever be correct, and no order-side
mitigation exists.

## 4. Traceability

- **PRD**: [`./PRD.md`](./PRD.md) — §13 dependencies, §15 open questions
- **DESIGN**: [`./DESIGN.md`](./DESIGN.md) §3.5; [`./design/03-gate-and-pin.md`](./design/03-gate-and-pin.md) §2.2; [`./design/06-workflow-seam.md`](./design/06-workflow-seam.md) §4.2, §4.6
- **Decisions**: [`./DECISIONS.md`](./DECISIONS.md) — D-32, D-56, Q-04, Q-05, Q-08
- **ADRs**: [`./ADR/0003`](./ADR/0003-cpt-cf-bss-orders-lifecycle-adr-fail-closed-gate.md) — the fail-closed posture that makes `SUB-O5` a blocker rather than a degradation
- **Upstream registers**: `gears/bss/subscriptions/docs/SEAMS.md` §I (`SUB-O1`…`SUB-O6`); the sibling Workflow PRD §13 (`SUB-O1`, `SUB-O5`…`SUB-O9`); `gears/bss/rating/docs/SEAMS.md` for the two Rating asks. Rating and the billing chain **are** specified in this repository — Rating carries a PRD, a DESIGN, ADRs and its own seam register, and the billing chain is specified as `gears/bss/ledger` — so both asks must be raised against those specifications rather than treated as unowned. **Payments alone has no specification and no register**, which is why that one ask is recorded here for whichever specification takes it; the same applies to a distinct tax owner, which `ledger` does not claim to be
