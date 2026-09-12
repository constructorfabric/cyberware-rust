<!-- CONFLUENCE_TITLE: [BSS]: Orders Workflow — Upstream Requirements -->
<!-- Related: ./PRD.md, ./DESIGN.md | Owners: BSS Orders team -->

# UPSTREAM_REQS — Orders Workflow

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Requesting Gears](#12-requesting-gears)
- [2. Requirements](#2-requirements)
  - [2.1 Subscriptions](#21-subscriptions)
  - [2.2 Payments](#22-payments)
  - [2.3 Generic Approval](#23-generic-approval)
  - [2.4 Orders Lifecycle](#24-orders-lifecycle)
  - [2.5 Catalog](#25-catalog)
  - [2.6 Privacy and data classification](#26-privacy-and-data-classification)
- [3. Priorities](#3-priorities)
- [4. Required PRD Amendments](#4-required-prd-amendments)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

This register captures the asks Orders Workflow raises on gears and specifications it does not
own: Subscriptions, Payments, and the Generic Approval service. It exists because these asks
previously lived only in PRD prose and forked from the canonical Subscriptions seam map
(`gears/bss/subscriptions/docs/SEAMS.md`). That map defines `SUB-O1` through `SUB-O6`, where
`SUB-O6` means **atomic multi-subscription submission is cheaper than the deferral assumed** (MED,
joint Orders/Subscriptions, reopening `SUB-D-04`/`SUB-C2`). The Orders Workflow PRD independently
reused `SUB-O6` for a **machine-readable in-flight rejection** — a completely different ask — and
invented `SUB-O7`, `SUB-O8`, `SUB-O9`, none of which were ever registered upstream. `SUB-O10` is
already claimed by the sibling Orders Lifecycle design for an explicit subscription-start instant.
The sibling Lifecycle register avoided the collision by declaring `SUB-O4`, `SUB-O6`, `SUB-O7` and
`SUB-O8` "not asks of that gear" — it could do so because all four are, in fact, asks of **this**
gear. Resolving the fork is therefore this gear's responsibility, done here by treating `SEAMS.md`
as canonical and renumbering this gear's four PRD-local asks to `SUB-O11`..`SUB-O14`, each labelled
**UNASKED** (never registered upstream, a stronger and more honest status than "unagreed").

This register also carries two inherited Subscriptions asks already present in `SEAMS.md`
(`SUB-O1`, `SUB-O5`, both unagreed), the sibling Lifecycle's `SUB-O10` precedent (which this gear
also depends on), a Payments ask with no owning specification anywhere in this repository, and a
Generic Approval ask against the PRD's own §9.2 expectations contract, which stands in as the
normative interface until a canonical specification exists. Two further Subscriptions asks,
`SUB-O15` and `SUB-O16`, are raised here for the first time — they are new asks at the next free
numbers, not renumberings of anything. It further carries asks on **Orders
Lifecycle** (visibility of the `submitted` TTL, which a normative MUST in this design depends on
and which this gear cannot see), on **Catalog** (the dependency-topology read every fulfillment
plan is constructed from, a dependency this register did not previously name at all), and on the
**PRD owner** for a privacy and data-classification ruling this design cannot make for itself.

### 1.2 Requesting Gears

| Upstream target | Agreement status | Why this gear needs it |
|------------------|-------------------|-------------------------|
| Subscriptions (`gears/bss/subscriptions/docs/SEAMS.md`) | `SUB-O1`, `SUB-O5` registered-and-unagreed; `SUB-O10` registered by the sibling Lifecycle design, unagreed; `SUB-O11`..`SUB-O16` UNASKED (never registered) | Provisioning intents, compensation, in-flight status, correlation propagation, the seam's latency budget, and callback attribution all cross this seam; several gaps make parts of the design fail closed or unenforceable until they land. |
| Payments | No specification or register exists in this repository | Begin-fulfillment gating needs an authorization outcome distinguishing authorized/pending/failed; there is no owner to receive the ask. |
| Generic Approval service | No canonical specification; PRD §9.2 expectations contract is the normative interface until one exists | Approval-requirement verdict acquisition, routing, multi-party gates, and escalation are executed against this contract via a phase-1 stand-in that returns `approval not required` (audited), pending the real service. |
| Orders Lifecycle (`gears/bss/orders-lifecycle`) | UNASKED (never registered) | This design's escalation obligation is stated relative to the order's `submitted` TTL, which Orders Lifecycle owns and this gear can neither read nor derive; without it the obligation is a strict inequality between two quantities, only one of which is knowable here. |
| Catalog | UNASKED (never registered; not a PRD-registered actor either) | Every fulfillment plan is constructed from Catalog's dependency topology and frozen against it; the plan cannot be built, validated for cycles, or ordered for compensation without a read contract. |
| PRD owner (privacy / data classification) | UNASKED | The PRD's "Privacy / PII: not applicable" exclusion does not survive contact with a 400-day audit trail carrying actor, operator and approver identities; the ruling is a PRD amendment, not a design change. |

## 2. Requirements

### 2.1 Subscriptions

The canonical seam-map numbering (`SUB-O1`..`SUB-O6`, per `gears/bss/subscriptions/docs/SEAMS.md`
lines 145-165) is treated as authoritative. This gear's PRD §13 previously defined `SUB-O6`
through `SUB-O9` locally; those four numbers are **renumbered and replaced** below as `SUB-O11`
through `SUB-O14`, not aliased. See the translation table in §2.1 ("Translation table") for the old-to-new mapping.

#### Overlap-presence read (inherited, unagreed)

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-overlap-presence-read`

Subscriptions **MUST** expose an overlap-key presence read: given an overlap scope key and a
payer, does a non-terminal subscription already hold that key, and what is the effective
concurrent-active cardinality. Orders Workflow consumes this at fulfillment-plan construction and
again immediately before the first activation intent (pre-activation abort check, PRD §6.1). This
gear cannot answer it alone because Subscriptions owns subscription state; the check is only
partially evaluable without it.

- **Rationale**: Registered upstream in `SEAMS.md` as `SUB-O5` (HIGH, "neighbour-extends"),
  unagreed. Until it lands, the against-existing-subscriptions half of the pre-activation abort
  check is unevaluable and therefore fails closed (halt before activation, void wave-1 drafts,
  record `overlap-collision`).
- **Consequence if it does not land**: the pre-activation overlap check can only ever evaluate the
  within-basket half; the against-existing-subscriptions half remains permanently unevaluable and
  the design must keep treating that half as a fail-closed refusal.
- **Source**: `gears/bss/subscriptions/docs/SEAMS.md` §I (`SUB-O5`); consumed by PRD §6.1
  (Fulfillment Plan Construction, pre-activation abort) and `DESIGN.md` fulfillment-plan slice.

#### Compensation cancellation reason (inherited, critical, unagreed)

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-compensation-cancel-reason`

Subscriptions **MUST** add a cancellation reason value for order-fulfillment compensation
(`SubscriptionCancelled.reason` is today a closed enum), scoped **out** of the early-termination
class so it derives neither an ETF nor a credit. This gear cannot satisfy it alone because the
enum and its ETF/credit scoping are owned by Subscriptions.

- **Rationale**: Registered upstream in `SEAMS.md` as `SUB-O1` (CRIT, "neighbour-extends"),
  unagreed. The activated-cancel compensation leg (order fulfilment failing after one or more
  lines have already activated) needs a reason that is neither an early termination (no party
  defaulted, no fee owed) nor a plan-change supersession; no existing enum value fits.
- **Consequence if it does not land**: this gear's activated-cancel compensation is forced to
  reuse a reason value that wrongly derives an ETF/credit, or repurpose `saga_superseded`, which is
  reserved for the cancel+new pair of a plan change — either choice misrepresents the compensation
  on the audit trail and on billing-facing events.
- **Note**: reason values ride event payloads and downstream consumers key on them, so adding a
  value after Billing consumes the contract is a **breaking** change — this is the ask materially
  cheaper now than later.
- **Source**: `gears/bss/subscriptions/docs/SEAMS.md` §I (`SUB-O1`); consumed by PRD §6.4
  (cancellation fencing / compensation) and the activated-cancel leg of the fulfillment design.

#### Explicit subscription start instant (raised by sibling Lifecycle design, unagreed)

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-explicit-start-instant`

Subscriptions' `create` and activation intent **MUST** accept an explicit start instant and
**MUST NOT** derive the subscription start from any date carried on the order. This gear depends
on it as much as Lifecycle does: the activation intent this gear issues must carry the actual
activation instant, because a line deferred past its quoted service-activation date **MUST NOT**
be backdated (PRD §6.1, §6.2 wave-2 activation).

- **Rationale**: Registered upstream as `SUB-O10` by the sibling Orders Lifecycle design, unagreed
  — a new ask, not present in either the seam map or this gear's PRD numbering. Raised here
  because this gear is the one that actually issues the activation intent and therefore the one
  that must supply the instant.
- **Consequence if it does not land**: without an explicit start instant on the intent, a line
  deferred past its quoted activation date is either backdated (violating the PRD's no-backdating
  requirement) or the actual activation instant is lost, making billing and entitlement start
  dates unenforceable from the order side.
- **Source**: `gears/bss/orders-lifecycle/docs/UPSTREAM_REQS.md` §2.1
  (`upreq-subscription-start-instant`, `SUB-O10`); consumed by PRD §6.1/§6.2 wave-2 activation.

#### `SUB-O11` — machine-readable in-flight rejection (UNASKED)

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-in-flight-rejection`

Subscriptions **MUST** reject a resubmit of an already-accepted, in-flight transition request with
a machine-readable outcome that distinguishes "already accepted" from "not accepted", so a caller
retry can tell which case it is in without guessing from silence or a generic error.

- **Rationale**: Orders Workflow's retry budget applies only to intent-**submission** failures; an
  intent already accepted and in flight must never be retried as a resubmit. Today Subscriptions
  has no contract for this distinction, so a resubmit's rejection is not machine-readable.
- **Consequence if it does not land**: the caller-side duplicate protocol (PRD §6.3) cannot reliably
  distinguish an in-flight duplicate from a genuine submission failure, forcing the sweep-based
  status read (`SUB-O13`) to carry the entire disambiguation burden with no fast-path signal.
- **Agreement status**: **UNASKED** — this identifier has never existed in `SEAMS.md` at any
  number; it is not registered-and-unagreed, it has simply never been raised upstream.
- **Source**: replaces the PRD's local `SUB-O6` (§13 Dependencies, §6.3 retry/duplicate protocol).
  Raised against `gears/bss/subscriptions/docs/SEAMS.md`, which has no such entry today.

#### `SUB-O12` — cancel/void of an accepted transition request (UNASKED)

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-cancel-accepted-transition`

Subscriptions **MUST** expose a cancel-or-void operation for an accepted, in-flight transition
request, usable as the superseding action for that intent instead of a second submit.

- **Rationale**: Once `SUB-O11` lets Orders Workflow recognize an in-flight duplicate, the only
  correct next action is to supersede that accepted intent, not resubmit it. No such operation is
  contracted today.
- **Consequence if it does not land**: an accepted in-flight intent that needs to be abandoned
  (e.g., on order cancellation or amendment) has no supported path other than waiting for it to
  reach a terminal outcome on its own, which can stall compensation and cancellation fencing.
- **Agreement status**: **UNASKED** — never registered upstream at any number.
- **Source**: replaces the PRD's local `SUB-O7` (§6.3 retry/duplicate protocol, §6.4 cancellation
  fencing). Raised against `gears/bss/subscriptions/docs/SEAMS.md`, which has no such entry today.

#### `SUB-O13` — status-read of a non-terminal intent (UNASKED)

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-nonterminal-status-read`

Subscriptions **MUST** expose a status-read for a non-terminal provisioning intent, addressable
either by the transition-request identifier or by `orderId` + `orderVersion` + order-line
reference + wave.

- **Rationale**: the background reconciliation sweep (PRD §Intent Reconciliation Sweep) must, after
  an idempotency key ages out, confirm outcomes by lookup rather than resubmission; this is the
  read-only discovery mechanism cancellation fencing's "reconcile in-flight intents" step depends
  on.
- **Consequence if it does not land**: the sweep cannot drive aged-out intents to a terminal
  confirmation or failure by lookup, so unresolved intents can only be resolved by resubmission
  (which risks duplication) or by parking indefinitely.
- **Agreement status**: **UNASKED** — never registered upstream at any number.
- **Source**: replaces the PRD's local `SUB-O8` (§Intent Reconciliation Sweep, §6.3 caller-side
  duplicate protocol). Raised against `gears/bss/subscriptions/docs/SEAMS.md`, which has no such
  entry today.

#### `SUB-O14` — `correlationId` propagation toward Policy Engine / OSS (UNASKED)

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-workflow-upreq-correlation-propagation`

Subscriptions **MUST** propagate the process `correlationId` it receives on each intent along the
Subscriptions → Policy Engine → OSS path, so it can be echoed on confirmations and observed
end-to-end.

- **Rationale**: Orders Workflow **MUST NOT** invoke OSS Provisioning directly (R3) — the
  Subscriptions → Policy Engine → OSS path is the only permissible provisioning route — so an
  end-to-end acquisition trace depends entirely on Subscriptions relaying the identifier onward.
- **Consequence if it does not land**: an end-to-end trace of a single fulfillment attempt stops at
  the Subscriptions seam; diagnosing a failure inside the Policy Engine or OSS legs cannot be
  correlated back to the originating process instance without a manual join.
- **Agreement status**: **UNASKED** — never registered upstream at any number.
- **Source**: replaces the PRD's local `SUB-O9` (§13 Dependencies, §6.3). Raised against
  `gears/bss/subscriptions/docs/SEAMS.md`, which has no such entry today.

#### Translation table (PRD-local numbers → renumbered asks)

| Old PRD number (local, colliding/unregistered) | New number (this register) | Meaning | Status |
|---|---|---|---|
| `SUB-O6` (PRD-local: in-flight rejection) | `SUB-O11` | Machine-readable in-flight rejection | UNASKED |
| `SUB-O7` | `SUB-O12` | Cancel/void of an accepted transition request | UNASKED |
| `SUB-O8` | `SUB-O13` | Status-read of a non-terminal intent | UNASKED |
| `SUB-O9` | `SUB-O14` | `correlationId` propagation toward Policy Engine / OSS | UNASKED |

Note: `SUB-O6` in `SEAMS.md` (canonical) means **atomic multi-subscription submission is cheaper
than the deferral assumed** (MED, joint Orders/Subscriptions, reopening `SUB-D-04`/`SUB-C2`) — an
entirely different ask from the PRD's former local `SUB-O6` (in-flight rejection, now `SUB-O11`).
This register does not raise the canonical `SUB-O6` reopen as an ask of this gear; it is recorded
here only to make the double meaning explicit and to confirm the collision is fully resolved by
the renumbering above.

#### What the design cannot do until `SUB-O11`–`SUB-O14` land

All four are **UNASKED** — never raised upstream at any number, not merely unagreed. Stated once,
plainly, so no slice has to restate it and no reader of a single slice concludes the mechanism
exists:

| Ask | What is not possible until it lands |
|---|---|
| `SUB-O11` in-flight rejection | A retry cannot tell "already accepted, in flight" from "not accepted". The caller-side duplicate protocol has no fast-path signal and must infer from silence or a generic error, which is exactly the inference that produces double-provisioning. The whole disambiguation burden falls on `SUB-O13`, which is itself UNASKED. |
| `SUB-O12` cancel/void of an accepted transition | An accepted in-flight intent that must be abandoned — on cancellation, amendment, or supersession — has no supported abandonment path. It can only be waited out to its own terminal outcome, which stalls cancellation fencing and compensation for as long as the downstream takes. |
| `SUB-O13` non-terminal status read | The reconciliation sweep cannot confirm an outcome by lookup. After an idempotency key ages out, an unresolved intent can only be resubmitted (risking duplication) or parked indefinitely, so the sweep's read-only switch has nothing to read. |
| `SUB-O14` `correlationId` propagation | An end-to-end trace of a fulfillment attempt stops at the Subscriptions seam; a failure inside the Policy Engine or OSS legs cannot be correlated back to the originating process instance without a manual join. |

Two of these sit inside **mandatory** sequences rather than in optional hardening, which is the
part a reader is most likely to miss: `design/05-provisioning-intents.md` names `SUB-O13` as the
reconciliation sweep's confirmation mechanism, and `design/06-saga-and-compensation.md` names
`SUB-O12` as step 3 of the cancellation-fencing sequence — a sequence that same slice requires to
"run to completion" before any compensated outcome may be reported. Both sequences are therefore
specified against mechanisms that do not exist upstream and have never been asked for. Neither
slice may be read as evidence that they do.

#### `SUB-O15` — latency and throughput budget for the provisioning seam (UNASKED)

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-provisioning-latency-budget`

Subscriptions **MUST** commit to a latency and throughput objective for the provisioning-intent
seam: a p95 and p99 for accepting an intent and for confirming it, a sustained intent-submission
rate this gear may offer, and the rate-limit or back-pressure signal returned when that rate is
exceeded.

- **Rationale**: the PRD's 15-minute p95 for a standard order is measured over a window in which
  every leg is a Subscriptions round trip, and this design's own fulfillment slice concedes that
  dependency-chain depth dominates the measured interval. The step deadline is currently *derived
  downward* from the SLA rather than *negotiated against* a downstream objective, which means the
  gear has allocated itself a budget out of a total it does not control.
- **Consequence if it does not land**: a slow-but-healthy Subscriptions is converted by this gear
  into failed lines, manual tasks and compensations, because the only bound available is the
  self-imposed step deadline; there is no basis to distinguish "the downstream is within its
  objective and we budgeted wrongly" from "the downstream is failing its objective".
- **Agreement status**: **UNASKED** — no latency, throughput or rate-limit ask exists in
  `SEAMS.md` or anywhere in this register today.
- **Source**: PRD §7 (fulfillment SLA), §6.3 (provisioning intents). Raised against
  `gears/bss/subscriptions/docs/SEAMS.md`, which has no such entry.

#### `AUTH-O1` — resolve an approver principal to their assigned gate set (UNASKED)

**Owner**: platform authentication gateway / Account Management assignment directory.

**What this gear needs**: the `SecurityContext` issued to an approver **MUST** carry the set of
approval-gate assignments that principal holds, resolvable as an inverse query (principal →
assigned `gate_id` set), not only as a forward one (gate → party label).

**Why**: `09-read-and-authz.md` §4.1 scopes the approver inbox and the decision endpoint to "own
assigned requests' orders only". `owf_approval_gate` carries a `party_ref` label, not a principal.
Without the inverse query the only implementable filter is role-string matching, which returns
every gate of that party across every order and seller — precisely the unscoped-read defect the
same section forbids, and the one the sibling Lifecycle design set corrected in its own review.

**Status**: **UNASKED** — never registered against the platform auth gateway. Until it lands, the
approver inbox cannot enforce its stated scope predicate, and `PRD.md:320`'s "requests outside the
approver's scope MUST NOT be shown" is unenforceable rather than merely unimplemented.

#### `SUB-O16` — echo the identity envelope on every confirmation (UNASKED)

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-identity-envelope-echo`

Subscriptions **MUST** echo, on every confirmation and failure callback, the identity envelope it
received on the originating intent — the process `correlationId`, the idempotency key, and the
asserting principal — so a callback can be attributed to the intent that caused it without a
lookup, and so a callback that echoes nothing this gear issued is rejectable as unattributable.

- **Rationale**: confirmations and failure outcomes arrive over an event/callback transport that
  traverses no REST gateway and carries no service principal of its own, so the design has no
  stated mechanism by which an inbound confirmation is authenticated or attributed. Echoing the
  envelope is the cheapest attribution primitive, and it is the precondition for treating an
  unattributable callback as a delivery-level failure rather than silently acting on it.
- **Consequence if it does not land**: this gear must attribute callbacks by payload matching
  alone, which cannot distinguish a genuine confirmation from a replayed or forged one, and the
  PRD's requirement that system actors not be impersonable has no enforcement on the chosen
  transport.
- **Agreement status**: **UNASKED** — never registered upstream at any number.
- **Source**: PRD §6.3 (provisioning-intent confirmations), §9.1 (actor authenticity). Raised
  against `gears/bss/subscriptions/docs/SEAMS.md`, which has no such entry.

### 2.2 Payments

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-payment-authorization-outcome`

An **authorization outcome** for a payer, distinguishing **authorized**, **pending**, and
**failed** as three separate answers, consumed as a begin-fulfillment guard input (PRD §6.3,
`OrderAcceptanceRecorded` re-evaluation) and never stored as an order fact.

- **Owning upstream gear**: none. **Payments has no specification and no register anywhere in
  this repository**, so this ask has no owner to receive it. It is recorded here, following the
  precedent the sibling Lifecycle register set, for whichever specification eventually takes it.
- **Why this gear cannot satisfy it alone**: authorization is a payment-instrument concern outside
  this gear's domain; Orders Workflow only consumes the outcome as a precondition.
- **Consequence if it does not land**: only one payment ordering is expressible in this design —
  provision first, collect after, with authorization used as a risk check — which does not cover a
  self-service card checkout that must collect payment **before** provisioning.
- **Agreement status**: UNASKED — no specification exists to register it against.
- **Source**: PRD §6.3 (begin-fulfillment precondition), §13 Dependencies (Payments, `p1`).

### 2.3 Generic Approval

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-generic-approval-expectations-contract`

A canonical Generic Approval service satisfying the expectations contract this gear's PRD §9.2
already defines: accept an `OrderApprovalRequest` (order context including the named TCV figure,
gate identifier, idempotency key, `correlationId`); evaluate the approval requirement and
thresholds, supporting multi-party gates; accept and route the escalation command Workflow issues
on timer expiry; return an `OrderApprovalDecision` (approved/rejected, with reason, echoing
`correlationId`); be idempotent by request key; and answer the approval-**requirement** verdict
query keyed on `orderId` + `orderVersion`, cacheable per version.

- **Owning upstream gear**: Generic Approval service. **No canonical specification exists today**
  (the `gears/approval-service` gear PRD remains a stub); PRD §9.2 is the normative interface until
  one exists.
- **Why this gear cannot satisfy it alone**: approval routing and threshold configuration are
  policy-owner concerns this gear must not compute (Lifecycle R2); Orders Workflow is a consumer of
  the verdict, not its author.
- **Consequence if it does not land**: multi-party gates, escalation, the Approver Inbox, and PRD
  acceptance criteria #1-#4a (including #2a) remain inert. In the interim, the **phase-1 stand-in**
  behind the §9.2 contract is the deciding authority: it always returns `approval not required`
  (audited as stand-in), so all orders proceed as if approval were never required until the real
  service lands.
- **Agreement status**: UNASKED — no canonical specification exists to register this contract
  against; PRD §9.2 is a self-declared expectations contract, not an upstream-agreed one.
- **Source**: PRD §9.2 (External Integration Contracts), §6.2 (approval execution), §13
  Dependencies (Generic Approval service, `p1`).

### 2.4 Orders Lifecycle

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-submitted-ttl-visibility`

Orders Lifecycle **MUST** make the point at which a `submitted` order expires knowable to this
gear. **The preferred form is the per-order expiry instant**, not the policy value: Lifecycle
stamps `submitted_expires_at` on the order and echoes it on the `OrderSubmitted` event this gear
already consumes. A published configuration value for the TTL duration, or a contract-documented
constant, are acceptable fallbacks.

The per-order instant is preferred for three reasons. It needs no new read path, because the event
already crosses the boundary. It is immune to the policy being retuned after an order was
submitted — an in-flight order carries its own deadline rather than inheriting a value that may
have changed underneath it, and `orders_state_ttl_policy` is a mutable policy table. And it turns
this gear's escalation check from a startup assertion against a global into a per-order
computation, which is strictly more correct: two orders submitted either side of a policy change
have different deadlines and should escalate at different times.

- **Owning upstream gear**: Orders Lifecycle (`gears/bss/orders-lifecycle`).
- **Why this gear cannot satisfy it alone**: Lifecycle owns the order and its state machine. This
  gear acts on the order without owning it and cannot read, derive or safely assume the TTL.
- **Rationale**: this design carries a normative **MUST** — when an approval verdict cannot be
  obtained, the process parks and escalation **MUST** fire before the `submitted` TTL elapses
  (`ADR/0007`). Both the escalation threshold and the lead time before expiry are deliberately
  stated as functions of that TTL rather than as absolute numbers, precisely because a number
  fixed on this side would assume a value this gear cannot see. The relational form is only
  implementable if the TTL is readable.
- **Consequence if it does not land**: the MUST has no path to satisfiability. The assertion this
  design requires — that the escalation lead time is present, positive, and strictly less than the
  remaining time before expiry — cannot be evaluated, so the escalation path is configured but
  never operating, and a parked order's only remaining bound is the TTL expiring with no prior
  alert. A commercial order is then lost to an outage in a third service.
- **Note — the upstream design already states the reciprocal constraint**: Orders Lifecycle's
  `design/07-hold-and-expiry.md:518` records that its `submitted` TTL "must exceed the sibling
  gear's escalation lead time, since expiry bounds the fail-closed park". Both gears have
  independently written down the same inequality from opposite sides, and neither can currently
  evaluate it, because the value sits in one gear's policy table and the consumer is in the other.
  This ask is the wire between two halves that already agree.
- **Interim in place — this ask is open but not blocking**: `DECISIONS.md` D-57 mirrors the TTL as
  local configuration under a startup refusal (absent, zero, negative or mis-ordered values are
  refused at configuration load). The escalation path is therefore operable today. What the mirror
  cannot do is notice that Lifecycle has retuned the real value, which is the residual risk this
  ask closes and the reason it stays open.
- **Agreement status**: **UNASKED** — no Orders Lifecycle ask has previously been registered by
  this gear at any number.
- **Source**: `ADR/0007` (`cpt-cf-bss-orders-workflow-adr-fail-closed-verdict-park`); `DECISIONS.md`
  D-46 and Q-02. Raised against `gears/bss/orders-lifecycle/docs/UPSTREAM_REQS.md`.

### 2.5 Catalog

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-catalog-dependency-topology-read`

Catalog **MUST** expose a read contract returning, for the product/offer references on an order's
lines, the **dependency topology** between them: which line's provisioning must precede which, the
cardinality of each edge, whether a bundle resolves to one provisioning unit or several, and a
stable identifier per node so the resolved topology can be frozen against an `orderId` +
`orderVersion` and replayed identically. The read **MUST** be versioned, so a topology change
cannot retroactively alter a frozen plan.

- **Owning upstream gear**: Catalog. It is not a PRD-registered actor for this gear, which is part
  of the gap: the fulfillment-plan slice already flags the dependency as real while the register
  named it nowhere.
- **Why this gear cannot satisfy it alone**: product structure and dependency relationships are
  Catalog-owned facts. This gear can neither invent them nor derive them from the order document,
  which carries line items, not their provisioning order.
- **Rationale**: plan construction, cycle validation, wave assignment and the reverse-order
  compensation walk are all defined over this topology. Without a contract the design depends on
  an undocumented read whose shape, stability and versioning are unknown, and a frozen plan cannot
  be shown to be reproducible.
- **Consequence if it does not land**: the fulfillment plan is constructed against an unspecified
  interface, dependency cycles cannot be rejected at construction time with any guarantee, and a
  mid-flight Catalog change can silently alter the topology a frozen plan was built from —
  producing a compensation walk that unwinds in an order the forward run never used.
- **Agreement status**: **UNASKED** — Catalog appears nowhere in this register today and has no
  seam map this gear can register against.
- **Source**: PRD §6.1 (Fulfillment Plan Construction); `DECISIONS.md` D-16;
  `design/04-fulfillment-plan.md` §3.6.

### 2.6 Privacy and data classification

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-workflow-upreq-pii-classification-ruling`

The PRD owner, with the privacy owner, **MUST** replace the PRD's "Privacy / PII: not applicable"
exclusion with a positive data-classification ruling covering the identifiers this design actually
retains, and **MUST** state the lawful basis under which a ≥ 400-day audit retention answers an
erasure request.

- **Owning upstream gear**: none — this is a **PRD amendment** and a privacy-owner ruling, out of
  scope for the design set. It is recorded here rather than resolved in the design, per the
  standing rule that the PRD is approved input.
- **Why this gear cannot satisfy it alone**: a data-classification and lawful-basis determination
  is a privacy-owner decision; a design that made it for itself would be asserting a legal
  position, not recording one.
- **Rationale**: the PRD excludes PII "beyond tenant and actor identifiers carried in the audit
  trail" — a carve-out whose exception swallows it. This design writes `owf_audit_entry.actor` on
  100 % of process transitions and retains it for at least 400 days, records operator identity and
  free-text justification on every override, and names the human `deciding_authority` who approved
  a commercial decision. That is personal data under any ordinary reading, held for over a year,
  under an append-only no-delete constraint this design deliberately hardened (`DECISIONS.md`
  D-50).
- **Consequence if it does not land**: the gear's retention posture and its erasure posture are in
  unresolved conflict, and the conflict is discovered at the first erasure request rather than at
  design time. The append-only, hash-chained audit store has no compliant deletion path by
  construction, so any answer that requires deleting rows would invalidate the design rather than
  configure it.
- **Agreement status**: **UNASKED** — no privacy owner has been named and no ruling requested.
- **Source**: PRD §8 (out-of-scope / privacy exclusion); `DECISIONS.md` D-50, D-38;
  `design/01-foundation.md` §3.7 (`owf_audit_entry`).

## 3. Priorities

| Priority | Requirements |
|----------|-------------|
| `p1` (critical) | `…-upreq-overlap-presence-read`, `…-upreq-compensation-cancel-reason`, `…-upreq-explicit-start-instant`, `…-upreq-in-flight-rejection`, `…-upreq-cancel-accepted-transition`, `…-upreq-nonterminal-status-read`, `…-upreq-provisioning-latency-budget`, `…-upreq-identity-envelope-echo`, `…-upreq-payment-authorization-outcome`, `…-upreq-generic-approval-expectations-contract`, `…-upreq-submitted-ttl-visibility`, `…-upreq-catalog-dependency-topology-read`, `…-upreq-pii-classification-ruling` |
| `p2` (important) | `…-upreq-correlation-propagation` |

## 4. Required PRD Amendments

1. **`SUB-O` renumbering.** PRD §13 Dependencies (lines 1161-1172) and every in-body citation of
   `SUB-O6`-`SUB-O9` **MUST** be updated to cite `SUB-O11`-`SUB-O14` respectively, per the
   translation table in §2.1 ("Translation table") above, and the text **MUST** state that the old `SUB-O6` collided
   with the canonical `SEAMS.md` meaning. The PRD is the current definition site for the colliding
   `SUB-O6` and the unregistered `SUB-O7`-`SUB-O9`, so it is the artifact that must carry the fix.

2. **`OrderAmended` approval-verdict re-obtaining gap.** PRD §6.2 (line 280) requires Workflow to
   cancel open approval gates for the prior version on `OrderAmended` and open new
   `OrderApprovalRequest`(s) for the new version when approval is required, but no PRD section
   requires **re-obtaining the approval-requirement verdict** for the new order version before
   doing so — it is implied, not stated. The PRD **MUST** add one or two sentences to §6.2 making
   explicit that on `OrderAmended`, Workflow **MUST** re-query the approval-requirement verdict
   keyed on the new `orderId` + `orderVersion` (the same verdict-acquisition path used for
   `OrderSubmitted`) and reflect it onward from `submitted`, rather than assuming the prior
   version's verdict or the presence of open gates alone determines the new version's requirement.
   Note: the sibling Lifecycle register (`gears/bss/orders-lifecycle/docs/UPSTREAM_REQS.md` §2.6,
   `upreq-workflow-amendment-verdict`) raises this as an ask on this gear and frames it more
   strongly — as a `MUST` obligation this gear has not yet met — than the PRD text actually
   supports; the PRD text only implies the re-obtain step through its gate-cancel/reopen language.
   This amendment closes that gap without a rewrite.

3. **Separation of duties on the approval gate.** PRD §9.2's expectations contract lists six
   clauses, none about separation of duties, and PRD §4 allows the Approver to be a seller
   operator — so an actor may submit an order and approve their own gate with every stated control
   passing. The PRD **MUST** add a clause **(g)** to the §9.2 expectations contract requiring the
   approval service to refuse a decision from the identity that submitted the order. This gear
   implements the local half today (`DECISIONS.md` D-56: the submitting identity is refused at the
   decision endpoint); the routing half belongs to the service and therefore to the contract the
   PRD owns.

4. **Privacy and PII classification.** See §2.6 above. The PRD's "Privacy / PII: not applicable"
   exclusion **MUST** be replaced with a positive data-classification ruling and a lawful-basis
   statement for the ≥ 400-day audit retention. Recorded as an ask rather than as a design change,
   since the PRD is approved input.

## 5. Traceability

- **PRD**: [`./PRD.md`](./PRD.md) — §6.1 (Fulfillment Plan Construction), §6.2 (Approval
  execution, `OrderAmended`), §6.3 (Provisioning Intents, retry/duplicate protocol, Intent
  Reconciliation Sweep), §6.4 (Cancellation fencing), §9.2 (External Integration Contracts), §13
  (Dependencies)
- **Design**: [`./DESIGN.md`](./DESIGN.md)
- **Upstream registers**: `gears/bss/subscriptions/docs/SEAMS.md` §I (`SUB-O1`..`SUB-O6`,
  canonical); `gears/bss/orders-lifecycle/docs/UPSTREAM_REQS.md` §2.1 (`SUB-O10` precedent, fork
  resolution stance) and §2.6 (`upreq-workflow-amendment-verdict`, the ask this gear inherits)
- **Registers with no receiving document**: Catalog and Payments have no seam map or upstream
  register in this repository, and the privacy ask has no named owner; §2.2, §2.5 and §2.6 are
  recorded here for whichever specification or owner eventually takes them.
- **Design and decision sources for the new asks**: `ADR/0007` (Lifecycle `submitted` TTL);
  `DECISIONS.md` D-16 (Catalog topology), D-46 and Q-02 (relational escalation threshold), D-50 and
  D-38 (audit retention and the privacy ask)
