---
refs:
  - bss/manifest/vz-arch-manifest-bss-only.md
  - bss/prd/PRD-contracts-agreements-202601120119
  - bss/prd/PRD-orders-lifecycle-202608101404
  - bss/prd/PRD-orders-workflow-202608111157
  - bss/prd/PRD-plan-price-modeling-202605281200
  - bss/prd/PRD-product-sku-management-202606101924
  - bss/prd/PRD-subscriptions-entitlements-202601120119
  - bss/prd/PRD-tariffs-pricing-logic-202604011200
---

# PRD — Change Orders

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Architecture Alignment](#2-architecture-alignment)
- [3. Actors](#3-actors)
  - [3.1 Human Actors](#31-human-actors)
  - [3.2 System Actors](#32-system-actors)
- [4. Operational Concept & Environment](#4-operational-concept--environment)
  - [4.1 Module-Specific Environment Constraints](#41-module-specific-environment-constraints)
- [5. Scope](#5-scope)
  - [5.1 In Scope](#51-in-scope)
  - [5.2 Out of Scope](#52-out-of-scope)
- [6. Functional Requirements](#6-functional-requirements)
  - [6.1 Change Order Document](#61-change-order-document)
  - [6.2 Sellability and Pricing of the Delta](#62-sellability-and-pricing-of-the-delta)
  - [6.3 Approval and Commitment](#63-approval-and-commitment)
  - [6.4 Application of the Increase](#64-application-of-the-increase)
  - [6.5 Events and Audit](#65-events-and-audit)
  - [6.6 Boundary with the Acquisition Path](#66-boundary-with-the-acquisition-path)
- [7. Non-Functional Requirements](#7-non-functional-requirements)
  - [7.1 NFR Inclusions](#71-nfr-inclusions)
  - [7.2 NFR Exclusions](#72-nfr-exclusions)
- [8. Five Quality Vectors Analysis](#8-five-quality-vectors-analysis)
- [9. Public Library Interfaces](#9-public-library-interfaces)
  - [9.1 Public API Surface](#91-public-api-surface)
  - [9.2 External Integration Contracts](#92-external-integration-contracts)
- [10. Use Cases](#10-use-cases)
  - [UC-001 - Partner Buys Additional Capacity](#uc-001---partner-buys-additional-capacity)
  - [UC-002 - Adding a Component to a Running Subscription](#uc-002---adding-a-component-to-a-running-subscription)
  - [UC-003 - Change Order Fails to Apply](#uc-003---change-order-fails-to-apply)
- [11. User Interaction and Design](#11-user-interaction-and-design)
- [12. Acceptance Criteria](#12-acceptance-criteria)
  - [Change Order Capture](#change-order-capture)
  - [Pricing of the Delta](#pricing-of-the-delta)
  - [Overlap and Eligibility](#overlap-and-eligibility)
  - [Application](#application)
  - [Non-Functional Requirements (Show-Stoppers)](#non-functional-requirements-show-stoppers)
- [13. Dependencies](#13-dependencies)
- [14. Assumptions](#14-assumptions)
- [15. Open Questions](#15-open-questions)
- [16. Risks](#16-risks)
- [17. Reference Materials](#17-reference-materials)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

This PRD ships the **`change` order category** for **increases**: a commercially initiated purchase that adds capacity or adds a component to a subscription that already exists, without replacing it.

It is the second phase of the Orders capability. The first phase (`PRD-orders-lifecycle-202608101404`, `PRD-orders-workflow-202608111157`) specified new acquisitions, where every order line spawns a new subscription. Those documents declare the `change` category and **reject it until this path ships**; this PRD is that path. The order document, its state machine, and the Lifecycle↔Workflow seam are **not** re-specified here — they are adopted by reference and extended only where an increase differs from an acquisition.

**Increase only.** Decreases, removals, and plan changes are out of scope this phase (§5.2, §15). The commercial motion in scope is the one a buyer recognises as "buy more".

### 1.2 Background / Problem Statement

A tenant that already has a running subscription has no commercial artifact for buying more. The order document exists only for acquisition, so the two most ordinary growth motions have no home:

- **Buy more of the same** — more capacity on an existing line. Today this is a subscription-side quantity change with no order behind it.
- **Add a component** — enable one more product or add-on for a tenant that already has a subscription. Today the only order-shaped route is a different bundle plan, which makes every sellable combination a pre-published catalog artifact and still cannot express "turn on one more thing".

The consequence is the one the Lifecycle PRD §1.2 already argues for acquisitions: without an order there is **no price pin, no approval gate, and no booking record**. Growth revenue — often the larger share — is transacted without the controls the platform applies to the first purchase.

Three questions deferred by the first phase converge here and are answered together: add-on selection on the order line, change-order phasing, and how a line targets a subscription that already exists.

### 1.3 Goals (Business Outcomes)

- A buyer can purchase additional capacity or an additional component against an existing subscription through the same commercial path as the original purchase — priced, gated, approvable, and audited.
- Every increase carries its own **catalog price pin** for the added quantity, so the price of growth is fixed at purchase and is not silently re-derived later.
- Approval policy sees the **incremental** value of the change, not the whole-contract value and not zero.
- An increase either applies in full or does not apply at all; a failed change order **MUST NOT** leave the target subscription partially modified.
- Adding a component to an existing subscription does not collide with the overlap rule that protects against duplicate active subscriptions.

### 1.4 Glossary

| **Term** | **Definition** |
|----------|----------------|
| **Change Order** | An order carrying `category = change`. Every line targets a subscription that already exists. A change order never spawns a subscription and never replaces one. |
| **Target Subscription** | The existing subscription a change line applies to, referenced by `targetSubscriptionId` on the line. Owned by Subscriptions; Orders references it and never mutates it directly. |
| **Change Kind** | The commercial shape of one change line. Two kinds this phase, both increases: **`increase_quantity`** (more of what the target already carries) and **`add_component`** (a product or add-on the target does not yet carry). |
| **Augment vs Supersede** | An increase **augments**: the target subscription survives with more on it. This is distinct from **supersession** (`supersedesSubscriptionId`), which Subscriptions reserves for a cancel-and-replace pair — cross-currency, cross-region, or frequency change. A change order **MUST NOT** be modeled as a supersession. |
| **Change Delta** | What the change order adds, per line: an added quantity, or an added component with its own quantity. The delta — not the resulting total — is what is priced, approved, and recorded. |
| **Delta Price Pin** | The catalog price pin captured on a change line at submit, covering the added quantity only. Structurally the same artifact as the acquisition pin (Lifecycle §1.4); commercially it is the price of growth, not of the original purchase. |
| **Delta TCV** | The named net pre-tax figure exposed to approval policy for a change order: the total-contract value of the **added** items over the remaining term, never the value of the target subscription as a whole. |

## 2. Architecture Alignment

| **Field** | **Value** |
|-----------|----------|
| **Applicable Manifest(s)** | BSS |
| **Relevant Chapters** | §4.6 Contracts and Agreements — §4.6.1 Orders (sub-area); §4.3 Subscriptions and Entitlements (composition, `PlanLink` / `AddOn`); §4.1 Product and Service Catalog (add-on rules); §3.1 Capability inventory; §8.2 Tenant axes |

> **Normative alignment**: This PRD is additive under the existing Orders sub-area — it introduces no new capability row and no new gear. It **MUST NOT** contradict: (a) Orders Lifecycle as the SoR for the order document and order state; (b) Subscriptions as the SoR for subscription state and composition; (c) the pricing domain as the SoR for price data, add-on rules, and grandfathering cohorts; (d) BSS manifest §2.1.2 — BSS **MUST NOT** mutate OSS topology or bypass the Policy Engine.

> **Consequential amendment**: Orders Lifecycle §1.1 and §6.1 currently require the `change` category to be **rejected** until this path ships. Adopting this PRD **MUST** be accompanied by lifting that rejection in the Lifecycle PRD. Until both land together, the category stays rejected — a half-adopted pair would accept change orders that no downstream path can fulfil.

## 3. Actors

> **Note**: Actors are those of the Orders capability; roles unchanged from the acquisition path unless stated. IDs are scoped to this PRD.

### 3.1 Human Actors

#### Partner Admin

**ID**: `cpt-cf-bss-orders-changes-actor-chg-partner-admin`

**Role**: Places change orders on behalf of a customer tenant whose subscription already exists.
**Needs**: See what a target subscription currently carries; add capacity or a component against it; see the priced delta before committing.

#### Direct Customer

**ID**: `cpt-cf-bss-orders-changes-actor-chg-direct-customer`

**Role**: Self-service buyer increasing their own subscription.
**Needs**: A "buy more" path that shows the incremental price and takes effect without re-provisioning what already runs.

#### Seller Operator

**ID**: `cpt-cf-bss-orders-changes-actor-chg-seller-operator`

**Role**: Processes and monitors change orders within the seller tenancy.
**Needs**: Visibility of change orders against a given target subscription, including those in flight.

### 3.2 System Actors

#### Orders Lifecycle

**ID**: `cpt-cf-bss-orders-changes-actor-chg-orders-lifecycle`

**Role**: SoR for the change order document and its state. The state machine, versioning, cancel/hold, expiry, and the seam rules are those of the acquisition path — unchanged.
**Integration direction**: This PRD extends the order document and the submit gate owned there.

#### Orders Workflow

**ID**: `cpt-cf-bss-orders-changes-actor-chg-orders-workflow`

**Role**: Executes the change order: obtains the approval verdict, applies the increase through Subscriptions, and acknowledges the outcome to Lifecycle.
**Integration direction**: Bidirectional, as in the acquisition path.

#### Subscriptions

**ID**: `cpt-cf-bss-orders-changes-actor-chg-subscriptions`

**Role**: SoR for the target subscription. Applies the increase as a composition or quantity change and confirms it. Owns effective dating, proration triggers, and the resulting billing impact.
**Integration direction**: Outbound from Orders Workflow (change intent); inbound confirmation or failure.

#### Catalog and Pricing

**ID**: `cpt-cf-bss-orders-changes-actor-chg-catalog-pricing`

**Role**: SoR for published SKU/plan/price data, plan-scoped **add-on rules**, and grandfathering cohorts. Supplies the predicates the delta gate evaluates and the rows the delta pin freezes.
**Integration direction**: Inbound (consumed).

#### Generic Approval Service

**ID**: `cpt-cf-bss-orders-changes-actor-chg-generic-approval`

**Role**: Owns the approval-requirement verdict and routing for change orders, as for acquisitions. Evaluates against the delta TCV.
**Integration direction**: Outbound request, inbound decision — via Orders Workflow.

## 4. Operational Concept & Environment

### 4.1 Module-Specific Environment Constraints

No module-specific deviations — project defaults apply.

## 5. Scope

### 5.1 In Scope

| **Feature** | **Priority** | **Notes** |
|-------------|-------------|-----------|
| `change` category activated for increases: `increase_quantity` and `add_component` | `p1` | Requires the consequential amendment in Lifecycle §1.1/§6.1 (§2) |
| `targetSubscriptionId` on the order line, with augment (not supersede) semantics | `p1` | Target survives; `supersedesSubscriptionId` is a different mechanism and MUST NOT be used |
| Add-on selection on the order line, valid at acquisition capture **and** on a change line | `p1` | Closes the add-on gap the acquisition PRD left open; one field serves both moments |
| Single-target constraint: all lines of one change order address one target subscription | `p1` | Makes the increase applicable as one transactional intent — no saga (§6.4) |
| Delta sellability gate: pricing predicates plus add-on rule bounds, evaluated against the delta | `p1` | Add-on bounds become evaluable because the line now carries the selection |
| Delta catalog price pin per change line | `p1` | Price of growth fixed at submit; grandfathering authority stays with pricing |
| Delta TCV exposed to approval policy | `p1` | Incremental value only — never the target subscription's whole value, never zero |
| Overlap-rule exemption for `add_component` against its own target | `p1` | Same subscription, so no duplicate-active collision |
| Target eligibility check at submit and immediately before apply | `p1` | Non-terminal, not mid-change, owned by the same payer/seller axes |
| Change-order events carrying the target and the applied delta | `p1` | Consumers must not infer the delta by diffing subscription state |
| Preview of a change: delta gate result and delta total with no state created | `p2` | Same shape as acquisition Preview |

### 5.2 Out of Scope

- **Decreases and removals** (reducing quantity, removing a component) — out of scope this phase. Their financial consequences are credits, proration refunds, and early-termination treatment, which are owned by the billing chain and have no authored gear today. Tracked in §15.
- **Plan change** (moving the target to a different plan, in either direction) — different downstream mechanics (`changePlan`, comparability, possible cancel-and-replace) and not what "buy more" means. Tracked in §15.
- **Proration, credit, and invoice-impact math** → Subscriptions and the billing chain. This PRD requires that the increase be effective-dated and priced; it computes nothing.
- **Subscription composition semantics** — what it means for a subscription to carry an added component, and how entitlements follow, are owned by Subscriptions (`PlanLink` / `AddOn`).
- **System-driven transitions** (renewal, trial conversion, dunning) — unchanged; they produce no order, per the acquisition PRD.
- **Cancellation or termination of the target subscription** — a subscription-lifecycle concern, never a change order.
- **The order state machine, versioning, cancel/hold, expiry, and seam rules R1–R5** → Orders Lifecycle; adopted by reference, not restated.

## 6. Functional Requirements

> **Testing strategy**: All requirements verified via automated tests (unit, integration, e2e) unless otherwise noted.

### 6.1 Change Order Document

#### Change Category and Line Targeting

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-line-targeting`

An order with `category = change` **MUST** be accepted, and every one of its lines **MUST** carry a `targetSubscriptionId` identifying an existing subscription. A change line **MUST** additionally carry a **change kind**: `increase_quantity` or `add_component`. A line of kind `increase_quantity` **MUST** reference a product the target already carries and an added quantity; a line of kind `add_component` **MUST** reference a published `skuId`/`planId`/`priceId` the target does not yet carry, and its quantity. The change **MUST** be applied as an **augment** — the target subscription survives, retains its identity, and is not replaced. A change order **MUST NOT** be expressed as a supersession (`supersedesSubscriptionId`), which is reserved for cancel-and-replace pairs owned by Subscriptions. An order **MUST NOT** mix `category = change` lines with acquisition lines.

**Rationale**: The whole point of the motion is that the running service is untouched. Modeling it as a replacement would re-provision what already works and break continuity the buyer is paying to keep.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-orders-lifecycle`, `cpt-cf-bss-orders-changes-actor-chg-subscriptions`

#### Single Target per Change Order

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-single-target`

All lines of one change order **MUST** address the **same** target subscription. A basket spanning several targets **MUST** be rejected at submit with a machine-readable reason; the buyer places one change order per target.

**Rationale**: This is what makes an increase applicable as a single transactional intent (§6.4). With several targets, a partial failure would leave one subscription increased and another not, and the only compensation available would be a decrease — the motion this phase explicitly does not specify. The constraint buys atomicity without depending on an unauthored billing reversal path.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-orders-lifecycle`

#### Add-On Selection on the Order Line

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-addon-selection`

The order line **MUST** be able to express which plan-scoped **add-ons** were selected against its base plan, each with its own quantity. This field **MUST** be available on an acquisition line as well as on a change line — the same selection, captured at two different moments. A required add-on declared by the plan's add-on rules **MUST** be present on the line for the order to pass the gate; an add-on not declared eligible for the plan **MUST** be rejected.

**Rationale**: The acquisition PRD's submit gate validated add-on bounds against data the line could not carry, so the clause was removed and add-on capture deferred. This is where it returns: the field that lets a buyer add a component to a running subscription is the same field that lets them buy the plan with add-ons in the first place, and specifying it once avoids two divergent models.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-catalog-pricing`, `cpt-cf-bss-orders-changes-actor-chg-orders-lifecycle`

#### Target Eligibility

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-target-eligibility`

At submit, the target subscription **MUST** be non-terminal, **MUST** resolve under the same tenant axes as the change order (`resourceTenantId`, `payerTenantId`, `sellerTenantId`), and **MUST NOT** have another change order in flight against it. The submit-time read **MUST** capture the target's **revision identifier** — the version or composition token Subscriptions exposes for optimistic concurrency — and the change order **MUST** record it.

Eligibility **MUST NOT** be enforced by a caller-side re-check before apply. The expected revision identifier **MUST** be carried on the change intent as a **precondition**, and Subscriptions **MUST** validate it — together with target non-terminality and the add-on bounds of the resulting composition — **inside the transaction that commits the change** (`CHG-S2`, §9.2). A precondition mismatch **MUST** reject the intent with a machine-readable reason and **MUST** leave the target unmodified; Orders then fails the change order or re-derives it against the current revision.

**Rationale**: A caller-side check "immediately before apply" is a time-of-check-to-time-of-use gap, not a guard. Between that read and the commit the target can be cancelled, transferred, or independently recomposed — and the increase would then duplicate a component, exceed an add-on maximum, or land on a terminal subscription. Only a precondition evaluated by the owner of the data, in the same transaction as the write, actually closes it.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-subscriptions`, `cpt-cf-bss-orders-changes-actor-chg-orders-workflow`

### 6.2 Sellability and Pricing of the Delta

#### Delta Sellability Gate

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-delta-gate`

The submit gate for a change order **MUST** adopt the published pricing sellability gate by reference, exactly as the acquisition gate does, and evaluate it against the **delta** — the added quantity and the added components — not against the target subscription's resulting total. In addition it **MUST** validate, fail-closed: (a) target eligibility per §6.1; (b) the added quantity satisfies the purchase-quantity floor where the price row declares one; (c) **add-on rule bounds** (eligible add-on SKU, required/optional, min/max/step quantity) for every selected add-on, evaluated against the resulting composition of the target; (d) currency and region of the change are consistent with the target subscription's authoritative binding — a change **MUST NOT** introduce a second currency onto one subscription; (e) references resolve without duplication across lines. Rejection **MUST** carry a machine-readable business-level reason.

**Rationale**: Growth is sold from the same catalog under the same rules as the first purchase; a separate, weaker gate for increases is how stale and non-sellable rows reach live subscriptions. Evaluating add-on bounds against the resulting composition — rather than against the line alone — is what makes a max-quantity rule meaningful for a subscription that already carries some.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-catalog-pricing`, `cpt-cf-bss-orders-changes-actor-chg-subscriptions`

#### Overlap Rule and Component Addition

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-overlap-exemption`

An `add_component` line **MUST NOT** be evaluated as an overlap collision against **its own target subscription**. The overlap rule protects against two concurrently active subscriptions on one scope key; adding a component to one subscription creates no second subscription and **MUST** be permitted where the resulting composition is otherwise valid. The rule continues to apply unchanged between the target and any **other** subscription.

**Rationale**: Without this exemption the change path would be self-blocking — the target already holds the scope key, so every component addition would be rejected as a duplicate of the very subscription it is modifying. This is also the correct answer to "enable one more product for a tenant that already has one", which under the acquisition path can only be attempted as a second order and refused.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-subscriptions`

#### Delta Price Pin

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-delta-pin`

On gate success the system **MUST** capture a **catalog price pin on every change line**, covering the added quantity and each added component. The pin **MUST** be taken from the catalog version current at change-order submit — the added quantity is a new purchase and is priced as one. Where the target subscription sits in a **protected grandfathering cohort**, whether the cohort price extends to the added quantity is determined by the **pricing domain**, which owns cohort membership and its price protection; Orders **MUST** consume that determination and **MUST NOT** infer, extend, or override it. The resulting pin **MUST** be recorded on the change order and **MUST** be carried to Subscriptions with the change intent.

**Rationale**: Whether growth is priced at yesterday's rate or today's is a commercial decision with real money attached, and leaving it unstated guarantees two implementations. Naming the authority — pricing, not Orders — keeps the decision in one place without this PRD asserting a pricing rule it does not own.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-catalog-pricing`

#### Resolved Delta Total

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-delta-total`

The system **MUST** capture a non-authoritative resolved total for the change, produced by the price-evaluation contract during the gate, structured as the acquisition total is (gross and net pre-tax, discount component, charge-kind decomposition, tax excluded). It **MUST** represent the **delta only**. The named figure exposed to approval policy is the **delta TCV**: the added items' net pre-tax value over the target's **remaining term**, or annualised where the term is open-ended. The total **MUST NOT** be used as a billing input.

**Rationale**: Approval thresholds are meaningless if a change order presents either the whole subscription's value or nothing. Scoping the figure to the remaining term is what makes two increases of different timing comparable.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-catalog-pricing`, `cpt-cf-bss-orders-changes-actor-chg-generic-approval`

### 6.3 Approval and Commitment

#### Approval of a Change Order

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-approval`

A change order **MUST** follow the same approval path as an acquisition: Orders Workflow obtains the approval-requirement verdict from the approval policy owner and reflects it into Lifecycle; the verdict is computed by neither Orders gear. The request context **MUST** carry the delta TCV, the target subscription reference, and the change kinds. Where the target is governed by a contract, the change **MUST** be evaluated against that contract's terms; where none is referenced, platform defaults govern.

**Rationale**: An increase is a commitment of the same kind as the original purchase and belongs under the same control. Reusing the policy owner keeps a single place where "what needs approving" is decided.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-generic-approval`, `cpt-cf-bss-orders-changes-actor-chg-orders-workflow`

#### Buyer Acceptance

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-acceptance`

Where acceptance is required — by the governing contract or by platform default — a change order **MUST** record a customer-acceptance instant before the increase is applied, on the same terms as an acquisition. Self-service submit by the buyer **is** acceptance; a partner-placed change order **MUST** record the customer's acceptance separately.

**Rationale**: A partner increasing a customer's spend without recorded assent is the same exposure as placing the original order without it.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-partner-admin`, `cpt-cf-bss-orders-changes-actor-chg-direct-customer`

### 6.4 Application of the Increase

#### Single Transactional Application

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-atomic-apply`

Orders Workflow **MUST** apply the whole change order to the target subscription as **one transactional intent**, carrying every line of the order. The application **MUST** be all-or-nothing at the subscription: either every line's increase is in effect, or none is and the target is unchanged. A failure **MUST** leave no partial modification and **MUST** require no compensating action against the target. The intent **MUST** be idempotent under retry, and its identity **MUST** include the order and order version.

**Acceptance of the change intent by Subscriptions is the amendment and cancellation boundary**, playing the role the subscription-spawn signal plays for an acquisition. Before acceptance the change order **MAY** be amended or cancelled directly, and the target is untouched. From acceptance onward the system **MUST** reject both a direct cancel and an amendment, and the intent **MUST** be driven to a confirmation or a failure. A new version of the same change order **MUST NOT** be submitted while a prior version's intent is outstanding: differing identity prevents a duplicate from being absorbed, but it would not prevent **both** deltas from applying, and an increase applied twice is not recoverable within this phase's scope. A buyer who wants a different increase after acceptance places a **new** change order once the outstanding one has reached its outcome, and that order runs the gate and approval afresh.

Because application is transactional, there is no post-acceptance state in which the target is partially modified, and therefore no compensated-cancel path to specify.

**Rationale**: This is the requirement the single-target constraint (§6.1) exists to make achievable. The acquisition path can compensate a partial failure by voiding drafts, because nothing is live until activation. Here the target is live from the start, so the only available compensation would be a decrease — which this phase does not specify and which would drag in credit and proration semantics that have no authored owner. Requiring transactional application removes the need for a saga instead of building one on top of an unwritten reversal path.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-subscriptions`, `cpt-cf-bss-orders-changes-actor-chg-orders-workflow`

#### Failure Handling

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-failure`

A failed application **MUST NOT** immediately terminalize the change order. Because the transaction left the target unmodified, the order is **recoverable**: the system **MUST** hold it, record a machine-readable failure reason, and produce a tracked record for the fulfillment operator — the same 100% visibility requirement as the acquisition path. From that held state exactly two operator resolutions are available: **retry**, which re-derives the intent against the target's current revision (§6.1) and resubmits it, or **cancel**, which is permitted precisely because nothing was applied. There **MUST NOT** be a compensation step against the target subscription.

The order reaches a terminal state only on one of: a confirmed application (`completed`), an operator cancel from the held state (`cancelled`), or exhaustion of the remediation policy (`fulfillment_failed`). An outcome that cannot be determined **MUST** be reconciled by status read against the target, never assumed successful and never assumed failed.

**Rationale**: The cancellation boundary in §Single Transactional Application closes direct cancel at intent acceptance, which would otherwise read as "a failed order is immediately terminal and there is nothing to retry". Naming the held state resolves that: acceptance stops the buyer from cancelling underneath an in-flight commit, and a *failed* commit reopens the order to the operator, because a transaction that changed nothing leaves nothing to unwind.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-orders-workflow`

#### Effective Dating

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-effective-dating`

A change line **MAY** carry a requested effective date for the increase. Where absent, the increase is effective at application. Orders **MUST** carry the requested date to Subscriptions and **MUST NOT** compute what it implies for billing periods, proration, or invoice timing — those are owned downstream. A requested date in the past **MUST** be rejected at the gate.

**Rationale**: The buyer's question ("from when") belongs on the commercial document; the consequences belong to the systems that own periods and money.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-subscriptions`

### 6.5 Events and Audit

#### Change Order Events

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-events`

The order state events of the acquisition path apply unchanged to change orders. Their payloads **MUST** additionally carry the `targetSubscriptionId` and, on the completion event, the **applied delta** per line (change kind, added quantity, added components, delta pin reference). Consumers **MUST NOT** be required to infer what changed by diffing subscription state before and after. Every change order and its outcome **MUST** be recorded in the audit log with actor identity and timestamp.

**Rationale**: A consumer that has to reconstruct the delta from two snapshots gets it wrong the moment two changes land close together, and the booking record — the reason the order exists — becomes unreliable.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-orders-lifecycle`

### 6.6 Boundary with the Acquisition Path

#### Binding to the Existing Seam

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-fr-chg-boundary-binding`

The seam rules **R1–R5** are normatively owned by the Orders Lifecycle PRD and are **not restated here**; change orders comply with them unchanged. Specifically: order state is read from and driven through Lifecycle (R1); the approval verdict is owned by the policy owner (R2); all subscription mutation goes through Subscriptions and never directly to OSS (R3); Orders performs no price computation and treats pricing references as opaque (R4); downstream per-request status is never mirrored into order state (R5). This PRD adds no new seam and no new authority.

**Rationale**: The first phase's recurring review finding was the same rule restated in two documents and drifting. The change path is a new commercial motion over the existing seam, not a second seam.

**Actors**: `cpt-cf-bss-orders-changes-actor-chg-orders-lifecycle`, `cpt-cf-bss-orders-changes-actor-chg-orders-workflow`, `cpt-cf-bss-orders-changes-actor-chg-subscriptions`

## 7. Non-Functional Requirements

### 7.1 NFR Inclusions

#### Delta Gate Latency

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-nfr-chg-gate-latency`

The system **MUST** complete the delta sellability gate and resolved-delta evaluation at p95 < 2 s for a change order of up to 20 lines.

**Threshold**: p95 < 2 s

**Rationale**: The gate runs interactively on submit and on every Preview; a slow gate is felt as a slow "buy more" button.

#### Application Atomicity

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-nfr-chg-apply-atomicity`

The system **MUST** guarantee zero partially applied change orders: for every change order, the target subscription reflects either all of its lines or none.

**Threshold**: Zero partial applications

**Rationale**: A partially applied increase bills for capacity the buyer did not agree to, or delivers capacity the buyer is not billed for, with no reversal path specified this phase.

#### Change Idempotency

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-nfr-chg-idempotency`

The system **MUST** guarantee zero duplicate increases under concurrent or retried requests: a retried change order with the same idempotency key **MUST** produce exactly one durable increase.

**Threshold**: Zero duplicate increases

**Rationale**: A duplicated increase doubles the delivered capacity and the charge, and — unlike a duplicated acquisition — cannot be voided as an unactivated draft.

#### Audit Completeness

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-nfr-chg-audit-completeness`

The system **MUST** record 100% of change orders and their outcomes in the audit log, including the target subscription and the applied delta.

**Threshold**: 100% coverage

**Rationale**: Growth revenue is audited to the same standard as acquisition revenue; a gap here is a gap in the booking record.

### 7.2 NFR Exclusions

- **Billing correctness of the resulting invoice** — owned by the billing chain; this PRD requires only that the delta be priced and pinned.
- **Subscription-side latency of applying a composition change** — owned by Subscriptions.
- **Provisioning latency of the added capacity** — owned by OSS via Subscriptions.

## 8. Five Quality Vectors Analysis

| **Quality Vector** | **Show-Stopper Requirements** | **Rationale** |
|--------------------|-------------------------------|---------------|
| **Efficiency** | A change order reuses the acquisition gate, approval path, and event set; it introduces no parallel machinery. | Two commercial paths with two sets of rules is the failure mode this phase exists to avoid. |
| **Reliability** | An increase applies in full or not at all, with no compensating action against a live subscription. | Partial application has no specified reversal this phase; atomicity is the mitigation, not a saga. |
| **Performance** | Delta gate at p95 < 2 s; target eligibility re-checked immediately before apply without a second full gate run. | The motion is interactive and frequent; the re-check must be cheap or it will be skipped. |
| **Security** | The initiating actor's authority over the target subscription is verified at submit; a partner-placed increase records customer acceptance where required. | Increasing someone's spend is a privileged act and needs the same proof as placing their first order. |
| **Versatility** | Change kinds are an extensible set; decrease, removal, and plan change are additive later without altering the lines, pins, or events specified here. | The deferred motions are known, and the model must not have to be rebuilt to admit them. |

## 9. Public Library Interfaces

> **Note**: Shapes (request/response structures, event payloads) are defined in Design. This section specifies business operations only.

### 9.1 Public API Surface

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-interface-chg-ops`

**Description**: The Orders capability **MUST** expose the following business operations for change orders, in addition to those of the acquisition path:

| Operation | Description | Idempotency | Concurrency |
|-----------|-------------|-------------|-------------|
| Create change order | Open a `change` order in `draft` against a target subscription | Idempotency key REQUIRED | One in-flight change order per target |
| Preview change | Run the delta gate and delta evaluation over a prospective change with no state created; returns per-line gate results and the delta total including delta TCV | Read-only | — |
| Submit change order | Run the delta gate, capture delta pins and the delta total, transition to `submitted` | Idempotency key REQUIRED | One in-flight change order per target |
| Read change order | Retrieve the change order including target reference, change kinds, and applied delta where complete | Read-only | — |

**Breaking Change Policy**: Additive changes (new change kinds, new optional line fields) are non-breaking. Removing a change kind or a required field requires a major version bump.

**Stability**: unstable (pre-GA; expected to stabilize after co-review with the Subscriptions gear on the change-intent contract).

### 9.2 External Integration Contracts

#### Change Intent Contract

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-contract-chg-change-intent`

**Direction**: Required by Orders Workflow (intent to Subscriptions).

**Description**: Orders Workflow **MUST** submit the change order as a **single** intent carrying the target subscription and its **expected revision identifier**, every line's change kind, added quantity, added components, delta price pin, and requested effective date, plus the order reference (`orderId`, `orderVersion`) and the process correlation identifier. Subscriptions **MUST** validate the expected revision as a precondition and apply the change transactionally, and **MUST** confirm or fail as a whole, echoing the order reference and correlation identifier. Payload shape is defined in Design and the Subscriptions PRD.

**Compatibility**: Governed by the Subscriptions PRD breaking change policy.

#### Upstream Asks on Subscriptions

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-contract-chg-upstream-asks`

**Direction**: Required by this PRD (obligations on Subscriptions).

**Description**: This PRD requires the following of the Subscriptions gear. Asks are numbered in a **`CHG-S*`** namespace deliberately: the `SUB-O*` register maintained by the Subscriptions gear is authored there, and reusing its numbers from this side has already produced one collision (§16).

| Ask | Obligation |
|-----|------------|
| `CHG-S1` | Accept an **order reference** (`orderId`, `orderVersion`, `orderLineId`) on quantity-change and composition-change operations. The existing register covers `create` only; without this, a subscription increased by an order cannot be traced back to it. |
| `CHG-S2` | Accept a **transactional multi-item change** against one subscription — several quantity increases and component additions applied as one commit, confirmed or failed as a whole — guarded by an **expected-revision precondition** supplied by the caller. Target non-terminality, the precondition, and the add-on bounds of the resulting composition **MUST** be evaluated inside that transaction, not by the caller beforehand (§6.1). A mismatch rejects the intent with a machine-readable reason and leaves the target unmodified. |
| `CHG-S3` | Evaluate the **overlap rule with an exemption for the subscription being modified**, so a component addition is not rejected as a duplicate of its own target. |
| `CHG-S4` | Expose a **composition read** sufficient for the delta gate: what the target currently carries, plus the **revision identifier** the precondition in `CHG-S2` is expressed against, so add-on min/max/step bounds can be evaluated against the resulting composition rather than the line alone. |
| `CHG-S5` | Accept and honour a **requested effective date** on the change, and own its proration and billing consequences. |

**Compatibility**: To be reconciled with the Subscriptions gear register at co-review.

## 10. Use Cases

### UC-001 - Partner Buys Additional Capacity

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-usecase-chg-buy-more-capacity`

**Actor**: `cpt-cf-bss-orders-changes-actor-chg-partner-admin`

**Preconditions**: The customer tenant has an active subscription; the partner has authority over it.

**Main Flow**:
1. The partner opens a change order against the target subscription and adds a line of kind `increase_quantity`.
2. Preview returns the delta gate result and the delta total including delta TCV.
3. On submit, the gate passes; the delta price pin, the delta total, and the target's expected revision identifier are captured; the order moves to `submitted`.
4. The approval verdict is obtained against the delta TCV; approval is not required, so the order moves to `approved`.
5. Because the order was placed by a partner rather than the customer, and the governing terms require acceptance, the customer-acceptance instant **MUST** be recorded before application; the order waits until it is.
6. Workflow applies the increase as one transactional intent; Subscriptions validates the expected revision and confirms.
7. The order completes, carrying the target reference and the applied delta.

**Alternative Flows**:
- Acceptance is not required by the governing terms: step 5 is skipped and application follows approval directly.
- Gate fails on the purchase-quantity floor: submission is rejected with a machine-readable reason; no state is created.
- The target was recomposed or became terminal during approval: Subscriptions rejects the intent on the revision precondition; the target is untouched and the order is held for the operator to retry against the current revision or cancel.

### UC-002 - Adding a Component to a Running Subscription

- [ ] `p1` - **ID**: `cpt-cf-bss-orders-changes-usecase-chg-add-component`

**Actor**: `cpt-cf-bss-orders-changes-actor-chg-direct-customer`

**Preconditions**: The customer has an active subscription that does not yet carry the component.

**Main Flow**:
1. The customer adds a line of kind `add_component` referencing the published plan and its selected add-ons.
2. The delta gate evaluates add-on rule bounds against the target's resulting composition and passes.
3. Self-service submit records acceptance; the order is approved and applied as one intent.
4. The subscription now carries the component; the completion event names the added component and its pin.

**Alternative Flows**:
- A required add-on declared by the plan is missing from the line: the gate rejects the order.
- The addition would exceed a declared max quantity given what the target already carries: the gate rejects the order.

### UC-003 - Change Order Fails to Apply

- [ ] `p2` - **ID**: `cpt-cf-bss-orders-changes-usecase-chg-apply-failure`

**Actor**: `cpt-cf-bss-orders-changes-actor-chg-orders-workflow`

**Preconditions**: An approved change order is ready for application.

**Main Flow**:
1. Workflow submits the change intent; Subscriptions rejects it — on the revision precondition or otherwise — or the outcome is a confirmed failure.
2. The target subscription is unchanged: the transaction committed nothing.
3. Workflow records the machine-readable reason, **holds** the change order rather than terminalizing it, and creates a tracked operator record.
4. The operator either retries — the intent is re-derived against the target's current revision and resubmitted — or cancels the order, which is permitted because nothing was applied. No compensation runs against the target.
5. The order becomes terminal only on a confirmed application, an operator cancel, or exhaustion of the remediation policy.

## 11. User Interaction and Design

| **Interface Name** | **Role** | **Steps** | **Mockup Screen** |
|--------------------|----------|-----------|-------------------|
| Change order composer | As a partner admin, I want to add capacity or a component to a customer's subscription so that I can serve a growth request without disturbing running service | 1. Select the target subscription<br>2. Add capacity or choose a component with its add-ons<br>3. Review the priced delta and submit<br>4. Where the governing terms require it, obtain and record the customer's acceptance before the increase is applied | — |
| Buy-more (self-service) | As a direct customer, I want to increase my subscription so that I get more capacity immediately | 1. Choose what to add<br>2. See the incremental price<br>3. Confirm | — |
| Change orders on a subscription | As a seller operator, I want to see change orders against a subscription so that I can explain how it grew | 1. Open the subscription<br>2. Review change orders and their applied deltas | — |

## 12. Acceptance Criteria

**As a** buyer, **I want** to purchase additional capacity or components against a subscription I already have **so that** growth is priced, approved, and recorded like any other purchase.

### Change Order Capture

**1. Change line targets an existing subscription**
- **Given** a change order with a line carrying `targetSubscriptionId` and a change kind
- **When** the order is submitted
- **Then** the system **MUST** accept it under `category = change`
- **And** the target subscription **MUST** retain its identity — no supersession is created

**2. Mixed-category basket is rejected**
- **Given** an order carrying both a change line and an acquisition line
- **When** submission is attempted
- **Then** the system **MUST** reject it with a machine-readable reason

**3. Multi-target change order is rejected**
- **Given** a change order whose lines reference two different target subscriptions
- **When** submission is attempted
- **Then** the system **MUST** reject it with a machine-readable reason

**4. Required add-on missing**
- **Given** a line whose plan declares a required add-on
- **And** the line carries no selection for it
- **Then** the gate **MUST** reject the order

### Pricing of the Delta

**5. Delta pin captured at submit**
- **Given** a change order that passes the gate
- **When** it is submitted
- **Then** the system **MUST** capture a catalog price pin covering the added quantity on every line
- **And** the pin **MUST** be carried to Subscriptions with the change intent

**6. Grandfathering is not decided by Orders**
- **Given** a target subscription in a protected grandfathering cohort
- **When** the delta is priced
- **Then** the applicable price **MUST** be the one the pricing domain determines for the cohort
- **And** Orders **MUST NOT** infer, extend, or override that determination

**7. Delta TCV is incremental**
- **Given** a change order adding items to a subscription of substantially larger value
- **When** the approval-requirement verdict is obtained
- **Then** the named figure **MUST** be the added items' value over the remaining term
- **And** it **MUST NOT** be the target subscription's whole value, and **MUST NOT** be zero where the added items carry committed value

### Overlap and Eligibility

**8. Component addition does not collide with its own target**
- **Given** an `add_component` line whose product shares a scope key with the target subscription
- **When** the gate evaluates the overlap rule
- **Then** the target itself **MUST NOT** be counted as a collision
- **And** the rule **MUST** still apply against any other subscription

**9. Target changed between submit and apply**
- **Given** an approved change order carrying the target's expected revision identifier
- **And** the target has since been recomposed, transferred, or become terminal
- **When** the change intent is committed
- **Then** Subscriptions **MUST** reject it on the precondition, inside the transaction
- **And** the target **MUST NOT** be modified
- **And** a caller-side re-check before apply **MUST NOT** be relied on in its place

### Application

**10. All-or-nothing application**
- **Given** an approved change order with several lines
- **When** application fails on any line
- **Then** the target subscription **MUST** reflect none of the lines
- **And** no compensating action **MUST** run against the target

**10a. Cancellation boundary is intent acceptance**
- **Given** a change order whose change intent has not yet been accepted by Subscriptions
- **When** the buyer cancels
- **Then** the system **MUST** accept the cancellation and the target **MUST** be untouched
- **And** once the intent has been accepted, a direct cancel **MUST** be rejected and the order **MUST** be driven to its terminal outcome by the confirmation or failure

**11. Amendment after intent acceptance is refused**
- **Given** a change order at version `N` whose intent has been accepted by Subscriptions
- **When** an amendment to that order is attempted
- **Then** the system **MUST** reject it
- **And** a second intent for the same order **MUST NOT** be submitted while the version-`N` intent is outstanding
- **And** a further increase is placed as a **new** change order after the outstanding one reaches its outcome, running the gate and approval afresh

**11a. Retry of an unaccepted intent is idempotent**
- **Given** a change intent that was submitted but not accepted, retried with the same identity
- **Then** the system **MUST** produce exactly one durable increase

**12. Applied delta is carried on completion**
- **Given** a change order that applied successfully
- **When** the completion event is published
- **Then** it **MUST** carry the target reference and the applied delta per line
- **And** consumers **MUST NOT** need to diff subscription state to learn what changed

### Non-Functional Requirements (Show-Stoppers)

**13. Zero partial applications**
- **Given** any change order that reaches application
- **Then** the target **MUST** reflect all of its lines or none

**14. Delta gate latency**
- **Given** a change order of up to 20 lines
- **When** the gate and delta evaluation run
- **Then** they **MUST** complete at p95 < 2 s

## 13. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| Orders Lifecycle (`PRD-orders-lifecycle-202608101404`) | SoR for the order document and state; owns the seam rules and the `change` category declaration this PRD activates | `p1` |
| Orders Workflow (`PRD-orders-workflow-202608111157`) | Executes approval and applies the change intent | `p1` |
| Subscriptions (`PRD-subscriptions-entitlements-202601120119`) | SoR for the target subscription and its composition; owes `CHG-S1`…`CHG-S5` (§9.2) | `p1` |
| Plan & Price / pricing domain (`PRD-plan-price-modeling-202605281200`) | Add-on rules, sellability predicates, grandfathering cohorts, price rows the delta pin freezes | `p1` |
| Generic Approval service | Approval-requirement verdict for change orders; no canonical spec today — the acquisition path's stand-in applies unchanged | `p1` |
| Contracts (`PRD-contracts-agreements-202601120119`) | Governing terms where the target is contracted | `p2` |

## 14. Assumptions

- The acquisition path is in place; this PRD extends it and does not stand alone.
- Subscriptions can apply a composition or quantity change to a live subscription without re-provisioning what already runs.
- Proration, credit, and invoice impact of an increase are owned downstream and need no Orders-side requirement beyond effective dating.
- The approval stand-in behaviour of the acquisition path applies unchanged to change orders until the Generic Approval service is specified.

## 15. Open Questions

| **Question** | **Owner** | **Target Date** | **Answer** | **Date Answered** |
|--------------|-----------|-----------------|------------|-------------------|
| Decrease and removal: reducing quantity or removing a component is the mirror of this phase, but its financial consequences — credits, proration refunds, early-termination treatment — are owned by a billing chain with no authored gear. Does the decrease path wait for that gear, or ship earlier with the financial consequence delegated wholesale to Subscriptions? | Product (with Architecture) | 2026-11-30 | — | — |
| Plan change as an order: moving a target to a different plan is commercially adjacent to "buy more" but mechanically different (`changePlan`, comparability ranking, possible cancel-and-replace for cross-currency or frequency moves). Does it become a third change kind here, or its own phase? | Product (with Architecture) | 2026-11-30 | — | — |
| Grandfathering and growth: confirm with the pricing domain whether a protected cohort's price extends to quantity added later, and whether that answer varies by cohort. This PRD requires Orders to consume the determination (§6.2) but the determination itself does not exist yet. | Architecture (with pricing) | 2026-10-30 | — | — |
| Multi-target change orders: the single-target constraint (§6.1) buys atomicity cheaply. If a buyer motion genuinely needs one commercial document across several subscriptions, it requires either a cross-subscription transactional apply or a compensation path — both larger than this phase. Is there real demand? | Product | 2026-11-30 | — | — |
| Add-on selection on acquisition lines: this PRD specifies the field for both moments (§6.1), which means the acquisition PRD's add-on exclusion is lifted by adopting this document. Confirm the two land together, as with the `change` category rejection (§2). | Architecture | 2026-10-30 | — | — |
| One in-flight change order per target (§6.1): a strict serialization. Confirm it is acceptable operationally, or whether concurrent non-overlapping changes against one target must be permitted. | Product (with Design) | 2026-10-30 | — | — |

## 16. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| **Half-adopted pair**: this PRD activates the `change` category, which the Orders Lifecycle PRD currently requires to be rejected, and lifts its add-on exclusion. | If this document is adopted without the corresponding Lifecycle amendment, change orders are accepted by one document and rejected by another; if the amendment lands first, the category is open with no specified fulfillment. | §2 states the consequential amendment explicitly and requires the two to land together; §15 tracks confirmation. |
| **Transactional apply is an upstream ask, not an existing capability**: §6.4 requires Subscriptions to apply a multi-item change as one commit (`CHG-S2`). | If Subscriptions cannot commit transactionally, atomicity would have to be rebuilt as a saga over a live subscription — whose only compensating action is a decrease, which this phase does not specify. | The ask is stated explicitly in §9.2 and is a co-review item; the single-target constraint keeps the required transaction as small as possible. Should the ask be refused, the phase boundary is revisited rather than a compensation path invented. |
| **Upstream ask numbering collision**: the Subscriptions gear maintains its own `SUB-O*` register, and the Orders Workflow PRD's asks `SUB-O6`…`SUB-O9` either mean something different there or are unregistered. | Two documents referring to different obligations by the same identifier; an obligation believed to be registered upstream that is not. | This PRD numbers its asks in a separate `CHG-S*` namespace (§9.2) rather than extending a register it does not own. Reconciling the existing Workflow asks with the upstream register is tracked separately from this document. |
| **Grandfathering unstated at implementation time**: §6.2 names pricing as the authority, but the determination does not exist yet. | Implementations pick a default — almost certainly current catalog price — and a protected cohort silently loses its protection on growth. | The requirement is written so that Orders consumes rather than infers; §15 carries the question with pricing as co-owner and an earlier target date than the other deferrals. |

## 17. Reference Materials

| **Material** | **Link** | **Comments** |
|--------------|----------|--------------|
| BSS Architecture Manifest | `docs/bss/manifest/vz-arch-manifest-bss-only.md` | §4.6.1 Orders; §4.3 Subscriptions composition; §4.1 add-on rules; §8.2 tenant axes |
| Orders Lifecycle PRD | `docs/bss/prd/PRD-orders-lifecycle-202608101404/` | Normative owner of the order document, state machine, and seam rules R1–R5; declares the `change` category this PRD activates |
| Orders Workflow PRD | `docs/bss/prd/PRD-orders-workflow-202608111157/` | Approval execution and fulfillment; applies the change intent |
| Subscriptions PRD | `docs/bss/prd/PRD-subscriptions-entitlements-202601120119/` | Target subscription SoR; composition, overlap rule, effective dating |
| Plan & Price Modeling PRD | `docs/bss/prd/PRD-plan-price-modeling-202605281200/` | Add-on rules, sellability predicates, grandfathering cohorts |
| Contracts PRD | `docs/bss/prd/PRD-contracts-agreements-202601120119/` | Governing terms where the target is contracted |
| Tariffs PRD | `docs/bss/prd/PRD-tariffs-pricing-logic-202604011200/` | Price-evaluation domain producing the resolved delta total |
| gears / bss / orders-lifecycle PRD | [`gears/bss/orders-lifecycle/docs/PRD.md`](../../orders-lifecycle/docs/PRD.md) | Normative owner of the order document, state machine, and seam rules R1–R5. Declares the `change` category this PRD activates, and carries the add-on exclusion this PRD lifts (§2). |
| gears / bss / orders-workflow PRD | [`gears/bss/orders-workflow/docs/PRD.md`](../../orders-workflow/docs/PRD.md) | Approval execution and fulfillment; applies the change intent. Source for the intent-identity and idempotency contract this PRD extends to the change intent. |
| gears / bss / subscriptions PRD (canonical, informative) | [`gears/bss/subscriptions/docs/PRD.md`](../../subscriptions/docs/PRD.md) | Target subscription SoR. Source for: effective-dated composition (`PlanLink` / `AddOn`), the overlap rule and `overlapScopeKey` default, `supersedesSubscriptionId` semantics this PRD deliberately does **not** use, and quantity-change operations. Its [`SEAMS.md`](../../subscriptions/docs/SEAMS.md) is the consumer-obligation register the `CHG-S*` asks in §9.2 target. |
| gears / bss / pricing PRD (canonical, informative) | [`gears/bss/pricing/docs/PRD.md`](../../pricing/docs/PRD.md) | Source for: plan-scoped add-on rules (eligible SKU, required/optional, min/max/step), sellability predicates adopted by the delta gate, and grandfathering cohorts governing whether a protected price extends to added quantity (§6.2). |
| gears / bss / rating PRD (canonical, informative) | [`gears/bss/rating/docs/PRD.md`](../../rating/docs/PRD.md) | Price-evaluation core producing the resolved delta total; composition SoR for `pricingSnapshotRef`. |
| gears / bss / contracts PRD (canonical, informative) | [`gears/bss/contracts/docs/PRD.md`](../../contracts/docs/PRD.md) | Governing terms where the target subscription is contracted; first draft. |
