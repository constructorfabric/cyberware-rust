---
refs:
  - bss/manifest/vz-arch-manifest-bss-only.md
  - bss/prd/PRD-contracts-agreements-202601120119/PRD-contracts-agreements-202601120119.md
  - bss/prd/PRD-orders-lifecycle-202608101404/PRD-orders-lifecycle-202608101404.md
  - bss/prd/PRD-plan-price-modeling-202605281200/PRD-plan-price-modeling-202605281200.md
  - bss/prd/PRD-subscriptions-entitlements-202601120119/PRD-subscriptions-entitlements-202601120119.md
---

Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

# PRD — Contracts & Agreements

> **Status: first draft (2026-08-17).** Harvested from three sources: (a) the consumer-side
> obligations already enumerated by Subscriptions in `gears/bss/subscriptions/docs/SEAMS.md`
> **SUB-C1…C5**, which are today unfulfilled and governed by platform defaults; (b) the upstream
> `PRD-contracts-agreements-202601120119` in vhp-architecture, whose scope tables predate the
> current gear split; (c) the two asks raised by the Orders Lifecycle PRD. Ownership boundaries
> below reflect the **current** split (rating owns evaluation and true-up math, pricing owns the
> catalog, Subscriptions owns lifecycle) — not the January authorship, which assigned several of
> these to Contracts. Sections 7–17 are scaffolded and marked TBD.

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Architecture Alignment](#2-architecture-alignment)
  - [2.1 Terminology and Naming](#21-terminology-and-naming)
  - [2.2 Predecessor PRDs and Scope Migration](#22-predecessor-prds-and-scope-migration)
- [3. Actors](#3-actors)
  - [3.1 Human Actors](#31-human-actors)
  - [3.2 System Actors](#32-system-actors)
- [4. Operational Concept & Environment](#4-operational-concept--environment)
- [5. Scope](#5-scope)
  - [5.1 In Scope](#51-in-scope)
  - [5.2 Out of Scope](#52-out-of-scope)
- [6. Functional Requirements](#6-functional-requirements)
  - [6.1 Contract Document and Lifecycle](#61-contract-document-and-lifecycle)
  - [6.2 Terms Consumed by Subscriptions](#62-terms-consumed-by-subscriptions)
  - [6.3 Negotiated Price Overrides](#63-negotiated-price-overrides)
  - [6.4 Commitments and Prepaid Pools](#64-commitments-and-prepaid-pools)
  - [6.5 Ramps](#65-ramps)
  - [6.6 Booking, Acceptance, Eligibility](#66-booking-acceptance-eligibility)
  - [6.7 Event Publication](#67-event-publication)
- [7. Non-Functional Requirements](#7-non-functional-requirements)
- [8. Five Quality Vectors Analysis](#8-five-quality-vectors-analysis)
- [9. Public Library Interfaces](#9-public-library-interfaces)
- [10. Use Cases](#10-use-cases)
- [11. User Interaction and Design](#11-user-interaction-and-design)
- [12. Acceptance Criteria](#12-acceptance-criteria)
- [13. Dependencies](#13-dependencies)
- [14. Assumptions](#14-assumptions)
- [15. Open Questions](#15-open-questions)
- [16. Risks](#16-risks)
- [17. Reference Materials](#17-reference-materials)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

Contracts & Agreements is the **System of Record for the signed commercial relationship** between a
selling tenant and a paying tenant: the terms that govern what may be bought, at what negotiated
price, for how long, on what renewal and grace rules, and against what commitments. It owns the
**terms**; it owns no money, no catalog content, no subscription state, and no order document.

Every downstream commercial artifact resolves against it — a subscription reads renewal and grace
terms, rating applies the contract overlay as the highest-precedence price layer, an order is placed
under a contract, and billing reads payment terms.

### 1.2 Background / Problem Statement

The gear does not exist, and its absence is already load-bearing in two places.

**Subscriptions runs on platform defaults.** `SUB-C1` records that renewal terms (`autoRenew`, term
windows, notice ladder), the grace ladder and regional templates are assumed to be Contracts-owned,
that the upstream PRD never authored them, and that **until it does, the platform defaults govern** —
a 7-day grace and a 30/14/7/1 notice ladder. Every release without this gear hardens those defaults
into the implementation and into customer expectation.

**Orders has an unsatisfiable precondition.** The Orders Lifecycle PRD requires every order to
reference an active contract and evaluates buyer eligibility "under the referenced contract"; no
contract entity, identifier or event producer exists on either side, so the precondition cannot be
met as written.

Three further consumer-side obligations are recorded and unfulfilled: negotiated `PriceOverride`
windows (`SUB-C5`, consumed by rating as the step-5 overlay), committed multi-step ramps (`SUB-C2`),
and commitment pools (`SUB-C3`). `ContractSigned` is already declared as an **inbound** consumer
contract by Subscriptions — an event nobody emits.

### 1.3 Goals (Business Outcomes)

- Negotiated commercial terms are captured once, versioned, and resolvable by every downstream gear
  without restating them, replacing the platform defaults Subscriptions currently runs on.
- A negotiated price is expressible without cloning catalog content — as a contract-scoped override
  window consumed by rating at its highest precedence layer.
- Commitments and prepaid pools have an authoritative definition and balance owner, so true-up and
  drawdown compute against one source.
- A new acquisition can be placed under a contract, and the terms that decide whether a party may
  buy a given plan are answerable as a predicate rather than by convention.
- Signature, booking and acceptance instants are recorded once and are auditable for revenue
  recognition.

### 1.4 Glossary

| Term | Definition |
|------|------------|
| **Contract** | The signed agreement between a `sellerTenantId` and a `payerTenantId` governing terms for a bounded period. SoR for terms; not an order, not a quote, not an invoice. |
| **Contract version** | An immutable revision of contract content. A change to signed terms creates a new version; prior versions are retained for audit and for reproducing a past evaluation. |
| **Term window** | `[termStart, termEnd)` in UTC. Drives renewal evaluation, notice scheduling and commitment measurement windows. |
| **Notice ladder** | The ordered set of lead times at which renewal notices fire before `termEnd` (platform default: 30/14/7/1 days). Contract-overridable; regional templates may pin statutory minima. |
| **Grace ladder** | The post-failure tolerance period and its escalation steps before entitlement is withdrawn (platform default: 7 days). |
| **Regional template** | A jurisdiction-scoped set of default terms (notice minima, grace, cancellation rights) applied to contracts in a territory unless overridden. |
| **PriceOverride window** | A contract-scoped, non-overlapping `[effectiveFrom, effectiveTo)` price adjustment for a catalog scope key. Consumed by rating as the step-5 contract overlay (precedence: Contract > Partner overlay > Catalog base). Contracts authors the window; it does **not** evaluate it. |
| **Commitment** | A committed volume or spend over a measurement window, with a breach policy. Contracts is SoR for the definition; balance/drawdown and true-up math are owned downstream. |
| **Ramp** | A committed multi-step schedule of plan or quantity changes across a contract term. Contracts authors it; Subscriptions executes it as scheduled intents. |
| **Booking instant** | `contractEffectiveAt` — the signature/booking date used as the revenue booking reference. Distinct from service activation and from customer acceptance. |
| **Party eligibility** | The contract-scoped predicate answering whether a given payer may purchase a given plan. Consumed by the order-capture gate. |

## 2. Architecture Alignment

| **Field** | **Value** |
|-----------|----------|
| **Applicable Manifest(s)** | BSS |
| **Relevant Chapters** | §4.6 Contracts and Agreements; §4.1 (catalog scope keys the overrides bind to); §4.2 (overlay precedence at evaluation); §4.3 (terms consumed by subscription lifecycle); §2.1.3 Multi-tenant semantics; §8.2 tenant axes |

### 2.1 Terminology and Naming

Canonical gear names are pinned as in the sibling maps: **Pricing (Product Catalog)**, **Rating
(evaluation core + pipeline)**, **Subscriptions**, **Billing & Invoicing**, **Orders (Lifecycle +
Workflow)**. This gear is **Contracts & Agreements**; no drift to "CPQ", "Quotes" or "Agreements
Engine".

### 2.2 Predecessor PRDs and Scope Migration

The upstream `PRD-contracts-agreements-202601120119` assigned this gear several responsibilities
that the current split places elsewhere. Migration, normative:

| Upstream scope item | Now owned by | Contracts retains |
|---|---|---|
| Pricing hierarchy orchestration (Contract > PriceList > Catalog) | **rating** (step-4/5 overlay precedence) | authoring the contract-scoped override window |
| True-up reconciliation engine (shortfall/overage calculation) | **rating** (true-up math) | commitment definition, threshold, breach policy |
| Usage credit pool tracking / balance | **Billing/rating** (balance, drawdown) | pool definition and its contract linkage |
| Price snapshotting at signature | **pricing** pre-stamps the catalog subset; **rating** composes; **Subscriptions** freezes the market binding | the override ids that enter the composite |
| SLA definition and monitoring | **deferred** — see §15 | — |

## 3. Actors

### 3.1 Human Actors

#### Contract Manager

**ID**: `cpt-cf-bss-contracts-actor-contract-manager`

**Role**: Authors and amends contracts, negotiates override windows and commitments, records
signature.

#### Finance Analyst

**ID**: `cpt-cf-bss-contracts-actor-finance-analyst`

**Role**: Reads booking instants, commitment positions and term windows for revenue and forecasting.

### 3.2 System Actors

#### Subscriptions

**ID**: `cpt-cf-bss-contracts-actor-subscriptions`

**Role**: Consumes renewal terms, grace ladder, regional templates, ramps and acceptance clauses;
executes ramps as scheduled intents. Stores **evaluated** term fields at evaluation time for replay.

#### Rating

**ID**: `cpt-cf-bss-contracts-actor-rating`

**Role**: Consumes `PriceOverride` windows as the highest-precedence overlay and the commitment set
for true-up. Computes; this gear does not.

#### Orders

**ID**: `cpt-cf-bss-contracts-actor-orders`

**Role**: References a contract on every order and consumes the party-eligibility predicate at
order capture.

#### Billing & Invoicing

**ID**: `cpt-cf-bss-contracts-actor-billing`

**Role**: Consumes payment terms and the commitment/pool linkage for posting.

## 4. Operational Concept & Environment

No gear-specific deviations — project defaults apply.

## 5. Scope

### 5.1 In Scope

| Feature | Priority | Notes |
|---------|----------|-------|
| Contract document, parties, term window, status machine, versioning | `p1` | SoR; content immutable per version |
| Renewal terms (`autoRenew`, term windows, notice ladder) | `p1` | Replaces the SUB-C1 platform defaults |
| Grace ladder + regional templates | `p1` | SUB-C1; statutory minima per territory |
| Contract-scoped `PriceOverride` windows | `p1` | Non-overlapping per scope key; SUB-C5; rating step-5 input |
| Commitment definition (type, threshold, measurement window, breach policy) | `p1` | SUB-C3; balance and true-up owned downstream |
| Booking instant + acceptance clause | `p1` | SUB-C4 date trio; acceptance confirmation shape is design |
| Party eligibility predicate | `p1` | Consumed by the order-capture gate; the predicate Orders asserts today with no owner |
| Ramps (committed multi-step schedules) | `p2` | SUB-C2; executed by Subscriptions, not here |
| Payment terms | `p2` | Consumed by Billing |
| Contract events | `p1` | `ContractSigned` already declared inbound by Subscriptions |

### 5.2 Out of Scope

- **CPQ / configurators / negotiation workflow** — no quote artifact is authored here.
- **The order document and its state machine** — Orders Lifecycle.
- **Price computation of any kind** — rating; this gear authors override windows only.
- **Subscription state, renewal execution, entitlement issuance** — Subscriptions.
- **Invoice posting, dunning, balance/drawdown** — Billing.
- **Catalog authoring** (plans, prices, bundles) — pricing.
- **Approval routing for contract signature** — no generic approval service exists; see §15.

## 6. Functional Requirements

> **Content boundary**: FRs define WHAT must be governed, not data models or APIs. Schemas, error
> taxonomies and wire contracts belong to the design set.

### 6.1 Contract Document and Lifecycle

#### Contract document

- [ ] `p1` - **ID**: `cpt-cf-bss-contracts-fr-contract-document`

A contract **MUST** carry: the parties (`sellerTenantId`, `payerTenantId`, and the resource-tenant
scope it covers), a stable identifier and a human-readable number, a term window `[termStart, termEnd)`
in UTC, and a status. Content **MUST** be immutable per version; a change to signed terms **MUST**
create a new version with prior versions retained. Any downstream evaluation **MUST** be reproducible
against the version in force at the evaluated instant.

**Rationale**: Every downstream gear resolves terms as of an instant; without versioned immutability a
past charge cannot be reproduced.

**Actors**: `cpt-cf-bss-contracts-actor-contract-manager`

#### Contract lifecycle

- [ ] `p1` - **ID**: `cpt-cf-bss-contracts-fr-contract-lifecycle`

The lifecycle **MUST** distinguish at least: not-yet-effective, active, expired at term end, and
terminated before term end. Termination **MUST** record the initiating party and reason, since the
reason class determines downstream fee and credit treatment. The contract **MUST NOT** be deletable
once signed.

**Rationale**: Termination reason drives early-termination treatment downstream; expiry and
termination are commercially different outcomes.

**Actors**: `cpt-cf-bss-contracts-actor-contract-manager`

### 6.2 Terms Consumed by Subscriptions

#### Renewal terms

- [ ] `p1` - **ID**: `cpt-cf-bss-contracts-fr-renewal-terms`

The contract **MUST** author `autoRenew`, the renewal term window, and the notice ladder. These
supersede the platform defaults Subscriptions applies today (30/14/7/1 notices) for any subscription
bound to a contract. Where no contract governs, the platform defaults **MUST** continue to apply —
the transition **MUST** be explicit, not implicit in the presence of a contract row.

**Rationale**: `SUB-C1` is the specific unfulfilled obligation; a silent switch from defaults to
contract terms would change renewal behaviour for live subscriptions.

**Actors**: `cpt-cf-bss-contracts-actor-subscriptions`

#### Grace ladder and regional templates

- [ ] `p1` - **ID**: `cpt-cf-bss-contracts-fr-grace-regional`

The contract **MUST** author the grace period and its escalation steps, and **MUST** support
territory-scoped templates supplying defaults and statutory minima. A contract term **MUST NOT** be
allowed to fall below a template's statutory minimum for its territory.

**Rationale**: Grace is a customer-visible commitment with jurisdictional floors; a per-contract value
without a floor check would create unenforceable terms.

**Actors**: `cpt-cf-bss-contracts-actor-subscriptions`

### 6.3 Negotiated Price Overrides

#### Price override windows

- [ ] `p1` - **ID**: `cpt-cf-bss-contracts-fr-price-override-windows`

The contract **MUST** support non-overlapping `[effectiveFrom, effectiveTo)` override windows bound to
catalog scope keys. Windows **MUST** be non-overlapping per key, UTC-bounded, and versioned with the
contract. This gear **MUST NOT** evaluate, resolve or compute an overridden price — it authors the
window and publishes it; rating applies it as the highest-precedence overlay.

**Rationale**: A negotiated price must not require cloning catalog content, and must not create a
second pricing engine.

**Actors**: `cpt-cf-bss-contracts-actor-rating`

### 6.4 Commitments and Prepaid Pools

#### Commitment definition

- [ ] `p1` - **ID**: `cpt-cf-bss-contracts-fr-commitment-definition`

The contract **MUST** author each commitment's type (volume, spend, or term), its threshold, its
measurement window, and its breach policy. This gear is SoR for the **definition and its contract
linkage** only: balance, drawdown and true-up computation are owned downstream. Measurement windows
**MUST** align to the contract term window unless explicitly overridden.

**Rationale**: `SUB-C3` splits definition from math; a definition without a single owner produces
divergent true-up results.

**Actors**: `cpt-cf-bss-contracts-actor-rating`, `cpt-cf-bss-contracts-actor-billing`

### 6.5 Ramps

#### Committed schedule

- [ ] `p2` - **ID**: `cpt-cf-bss-contracts-fr-ramp-schedule`

The contract **MAY** author a ramp: an ordered set of intervals with the plan or quantity committed in
each. Contracts authors the schedule; **Subscriptions executes** it as scheduled change intents. This
gear **MUST NOT** hold execution state.

**Rationale**: `SUB-C2` already assigns execution; recording the schedule here keeps the commitment
auditable against the signed agreement.

**Actors**: `cpt-cf-bss-contracts-actor-subscriptions`

### 6.6 Booking, Acceptance, Eligibility

#### Booking instant and acceptance

- [ ] `p1` - **ID**: `cpt-cf-bss-contracts-fr-booking-acceptance`

The contract **MUST** record the signature/booking instant (`contractEffectiveAt`) as the revenue
booking reference, and **MUST** declare whether customer acceptance is required for services sold
under it. Booking, service activation and customer acceptance are **three distinct instants**; this
gear owns the first and the requirement for the third, Subscriptions stamps the second.

**Rationale**: `SUB-C4` records the date trio with the acceptance-confirmation shape open; conflating
booking with activation misstates revenue timing.

**Actors**: `cpt-cf-bss-contracts-actor-finance-analyst`, `cpt-cf-bss-contracts-actor-subscriptions`

#### Party eligibility predicate

- [ ] `p1` - **ID**: `cpt-cf-bss-contracts-fr-party-eligibility`

The contract **MUST** express which catalog scope the payer is entitled to purchase under it, and the
gear **MUST** expose that as a **predicate evaluable before any subscription exists** — the order
capture gate is its consumer. A negative outcome **MUST** carry a machine-readable business reason.

**Rationale**: The Orders gate asserts this check today with no owner anywhere; without a predicate it
degrades to convention.

**Actors**: `cpt-cf-bss-contracts-actor-orders`

### 6.7 Event Publication

#### Contract domain events

- [ ] `p1` - **ID**: `cpt-cf-bss-contracts-fr-contract-events`

The gear **MUST** publish, with idempotent consumer semantics: contract signed, renewed, amended
(carrying the new version), terminated, and expired; price-override window activated and expired; and
commitment threshold and breach signals. `ContractSigned` is already declared as an inbound consumer
contract by Subscriptions — this requirement makes it a real producer. Envelope and payload follow the
platform event standard; reason enums **MUST** be closed and versioned.

**Rationale**: Downstream gears consume terms by event plus read model; a missing producer is why the
platform defaults are still in force.

**Actors**: `cpt-cf-bss-contracts-actor-subscriptions`, `cpt-cf-bss-contracts-actor-rating`, `cpt-cf-bss-contracts-actor-billing`

## 7. Non-Functional Requirements

TBD. At minimum: term evaluation must be reproducible as of any past instant; override-window reads
sit on the rating hot path and need a stated latency class; contract records are financial-grade
retention.

## 8. Five Quality Vectors Analysis

TBD.

## 9. Public Library Interfaces

TBD. Expected surfaces: contract CRUD and versioned read; override-window read model for rating;
party-eligibility predicate for order capture; commitment definition read.

## 10. Use Cases

TBD.

## 11. User Interaction and Design

TBD.

## 12. Acceptance Criteria

TBD — to be derived per FR once §6 is reviewed.

## 13. Dependencies

| Dependency | Description | Criticality |
|---|---|---|
| Pricing (Product Catalog) | Scope keys the override windows bind to | `p1` |
| Rating | Consumes override windows and commitment definitions; owns all math | `p1` |
| Subscriptions | Consumes renewal/grace/template terms; executes ramps | `p1` |
| Orders Lifecycle | References contracts; consumes the eligibility predicate | `p1` |
| Billing & Invoicing | Consumes payment terms and pool linkage (gear unauthored) | `p2` |
| IdP / AMS | Party identity and tenant-type resolution | `p1` |

## 14. Assumptions

- Platform defaults remain in force for subscriptions not bound to a contract; this gear does not
  retroactively re-govern existing subscriptions on first release.
- No generic approval service exists; contract signature approval is out of scope until one does.

## 15. Open Questions

| Question | Owner | Notes |
|---|---|---|
| SLA definition and monitoring — in this gear, a separate one, or deferred entirely? | Architecture | The upstream PRD scoped it here at moderate complexity; no consumer has requested it. |
| Acceptance-confirmation flow — who confirms, what evidence is retained? | Product | `SUB-C4` leaves the shape open. |
| Migration from platform defaults — does binding a contract to a live subscription change its in-flight renewal terms, or only from the next term? | Product | Determines whether §6.2 is additive or behaviour-changing. |
| Does a contract scope multiple payers (a master agreement with child accounts), or strictly one? | Product | Affects the eligibility predicate and the order's tenant axes. |
| Ramp authorship vs the Subscriptions deferral (`SUB-D-04`) — reopen jointly? | Architecture | Cross-referenced from the Orders seam `SUB-O6`. |

## 16. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| Platform defaults harden before this gear lands | Renewal and grace behaviour becomes de-facto contract terms customers rely on; changing them later is a customer-visible change | Author §6.2 early and state the migration rule explicitly (§15). |
| Scope creep back toward CPQ | The gear grows a quote artifact and duplicates order capture | §5.2 excludes it; the order's draft state already serves the pre-submit basket need. |
| Override windows become a second pricing engine | Divergence from rating's resolution | §6.3 forbids evaluation here; only authorship and publication. |

## 17. Reference Materials

| Material | Link | Comments |
|---|---|---|
| Subscriptions seam map | `gears/bss/subscriptions/docs/SEAMS.md` | `SUB-C1…C5` are the consumer-side requirement list this draft harvests |
| Orders Lifecycle PRD + review | vhp-architecture `docs/bss/prd/PRD-orders-lifecycle-202608101404/` | `contractId` precondition and the eligibility predicate; review findings F-11, F-9 |
| Upstream Contracts PRD | vhp-architecture `docs/bss/prd/PRD-contracts-agreements-202601120119/` | Pre-split scope tables; see §2.2 migration |
| Pricing PRD | `gears/bss/pricing/docs/PRD.md` | Canonical scope key the override windows bind to |
| Rating PRD | `gears/bss/rating/docs/PRD.md` | Overlay precedence, true-up ownership |
