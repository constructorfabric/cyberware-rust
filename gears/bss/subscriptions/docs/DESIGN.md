Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Subscriptions — End-to-End Lifecycle — Technical Design (canonical index) -->
<!-- Related: ./PRD.md, ./SEAMS.md, ./DECISIONS.md, ./ADR/, ./design/ | Owners: BSS Subscriptions team -->

# Technical Design — Subscriptions (End-to-End Lifecycle)

<!-- toc -->

- [1. Architecture Overview](#1-architecture-overview)
  - [1.1 Architectural Vision](#11-architectural-vision)
  - [1.2 Architecture Drivers](#12-architecture-drivers)
  - [1.3 Architecture Layers](#13-architecture-layers)
- [2. Principles & Constraints](#2-principles--constraints)
  - [2.1 Design Principles](#21-design-principles)
  - [2.2 Constraints](#22-constraints)
- [3. Technical Architecture](#3-technical-architecture)
  - [3.1 Domain Model](#31-domain-model)
  - [3.2 Component Model](#32-component-model)
  - [3.3 API Contracts](#33-api-contracts)
  - [3.4 Internal Dependencies](#34-internal-dependencies)
  - [3.5 External Dependencies](#35-external-dependencies)
  - [3.6 Interactions & Sequences](#36-interactions--sequences)
  - [3.7 Database schemas & tables](#37-database-schemas--tables)
  - [3.8 Deployment Topology](#38-deployment-topology)
- [4. Additional context](#4-additional-context)
- [5. Traceability](#5-traceability)

<!-- /toc -->

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-design-main`

> **Canonical design entry point and index.** This document is the subscriptions gear's top-level
> technical design and the anchor for spec traceability. The design is authored as a **set of slice
> documents** under [`design/`](./design/): a shared **Lifecycle Foundation** (the subscription
> aggregate, the manifest-closed status machine, the `TransitionRequest` envelope with idempotency +
> ordering, effective-dated composition, the Policy/OSS gate, versioning + audit) plus per-capability
> slice designs. This page is the single index over that set — architecture overview, the slice map
> with PRD traceability, dependency order, the cross-cutting normative statements, the ADR index, and
> the traceability surface — and delegates slice-level specifics (schemas, sequences, event field
> matrices) to the slice documents so they stay the single source of truth for their detail.
>
> **Status**: skeleton — the overview, slice map, and cross-cutting normatives are authored
> pre-Design-lock; the slice documents are seeded against the seams in
> [`SEAMS.md`](./SEAMS.md) and are authored on the agreed cut below.

## 1. Architecture Overview

### 1.1 Architectural Vision

The **subscriptions gear** is the BSS **System of Record** for the subscription commercial
aggregate (manifest §4.3): its **lifecycle state machine**, effective-dated **composition**
(`PlanLink`/`AddOn`), the plan-change **boundary and mode**, **renewal** execution, **entitlement**
assignment + point-of-use check state, and **multi-tenant** ownership. Given a governed transition
request, the gear validates it, gates resource-affecting effects fail-closed through the Policy
Engine, coordinates OSS provisioning, commits an **idempotent, versioned** state change, issues or
revokes entitlements, and emits ordered lifecycle events — so that **rating** can charge, **Billing**
can post, and **OSS** can provision deterministically from committed subscription state
([`PRD.md`](./PRD.md) §1.1).

The gear is deliberately **not** an authoring catalog and it **computes no money**: the pricing gear
owns `Plan`/`Price`/`PriceWindow`/`PriceOverlay`/`CatalogVersion` and publish governance; the rating
gear owns tariff evaluation and all **proration math**; Billing owns posting, floor/cap, and
rounding. Subscriptions owns the **WHEN** of a change (`changeEffectiveAt`, `changeMode`) and never
the arithmetic — the single split that keeps replay deterministic ([`PRD.md`](./PRD.md) §6.3).

The cross-gear complementarity — the composition read-model + change boundary, the `(currency,
region)` snapshot segment, the plan-change classification and phase/grant contracts adopted from
pricing, the renewal/grace obligation on Contracts, the Billing recurring-idempotency and
`collectionPaused` lines, the Policy/OSS gate and entitlement enforcement split — is frozen in
[`SEAMS.md`](./SEAMS.md); every slice here implements the subscriptions side of a listed seam.

### 1.2 Architecture Drivers

Requirements from [`PRD.md`](./PRD.md) that significantly shape the architecture.

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-subscriptions-fr-status-enum` / `cpt-cf-bss-subscriptions-fr-transitions-guards` | A **closed, manifest-aligned** status machine (`draft`→`active`↔`suspended`→`cancelled`→`archived`) with terminality; every edge carries a guard, and the Foundation is the single place a transition commits ([`design/01-foundation-lifecycle.md`](./design/01-foundation-lifecycle.md)). |
| `cpt-cf-bss-subscriptions-fr-transition-request` / `cpt-cf-bss-subscriptions-fr-scheduled-intents` | A uniform `TransitionRequest` envelope (`type`, `idempotencyKey`, `status`) gives idempotency, approval hooks, and audit one shape; non-immediate intents (`cancelMode`, `resumeAt`) are **pending intents on the aggregate** visible to the renewal job (SUB-D-01). |
| `cpt-cf-bss-subscriptions-fr-effective-dated-composition` / `cpt-cf-bss-subscriptions-fr-snapshot-discipline` | `PlanLink`/`AddOn` are **effective-dated intervals**, never destructive edits; fee artifacts carry `pricingSnapshotRef` with the Subscriptions-written `(currency, region)` segment so posted periods reproduce from frozen inputs (SUB-R2). |
| `cpt-cf-bss-subscriptions-fr-plan-change-boundary` / `cpt-cf-bss-subscriptions-fr-proration-ownership` | Subscriptions sets `changeEffectiveAt`/`changeMode` and emits `SubscriptionPlanChanged`; the **rating gear** owns proration math and usage slicing at the same boundary (SUB-R1). |
| `cpt-cf-bss-subscriptions-fr-renewal-evaluation` / `cpt-cf-bss-subscriptions-fr-grace-policy` | A Contract-driven renewal job with a testable **grace ladder** (7-day default, paused next-term recurring, hybrid exit) storing **evaluated fields** for replay; Contracts is the SoR the job consumes (SUB-C1). |
| `cpt-cf-bss-subscriptions-fr-entitlement-check-contract` / `cpt-cf-bss-subscriptions-fr-entitlement-assignment` | Entitlements are **assigned** from the pricing published grant set (incl. per-phase map, D-41); the gear serves a real-time **check decision state** (p95 < 100ms) while **OSS enforces** (SUB-P2, SUB-E3). |
| `cpt-cf-bss-subscriptions-fr-recurring-idempotency` / `cpt-cf-bss-subscriptions-fr-no-retro-edit` | `BillableItem(kind=recurring)` idempotent per `(subscriptionId, billing period, lineKey)`; posted invoices immutable — corrections are new billable/adjustment artifacts (SUB-B1). |
| `cpt-cf-bss-subscriptions-fr-event-ordering` / `cpt-cf-bss-subscriptions-fr-event-payload-completeness` | Ordered per `(orderingTenantId, subscriptionId)` — pinned at creation, immutable across transfers (SUB-D-06); composition-changing events carry enough snapshot-oriented context for rating + Billing to stay aligned and idempotent — the shared ordering key with rating (SUB-R1). |
| `cpt-cf-bss-subscriptions-fr-delegation-proofs` / `cpt-cf-bss-subscriptions-fr-tenant-axes` | Three tenant axes referenced from AMS (never invented); cross-tenant mutation requires an auditable **delegation proof** or is rejected. |

#### NFR Allocation

Non-functional targets are in [`PRD.md`](./PRD.md) §7.1; the SLA baselines are **carried from the
predecessor** and MUST be reconciled with the program NFR workshop (the workshop overrides on
conflict — §14).

| NFR theme | Allocated to | Design Response | Status |
|-----------|--------------|-----------------|--------|
| Lifecycle control-plane latency (p95 < 1s) | Foundation transition path | Synchronous commit class = the **intent commit** (validate + idempotency + guard + Policy pre-check + single versioned write); **OSS confirm is never inline** — OSS-blocking edges run async (`pending → approved → applied`) and only their intent commit is bounded (slice 01 NFR row is the authority) | Baseline (workshop-pending) |
| Entitlement check latency (p95 < 100ms) | Entitlement check read surface | Cache-friendly, tenant-isolated projection served off the hot path; propagation to the check surface < 5s | Baseline (workshop-pending) |
| Recurring generation cut (daily by 00:00) | Recurring emission job | Idempotent per `(subscriptionId, billing period, lineKey)`; coordinated singleton | Baseline (workshop-pending) |
| Proration monetary alignment (100%) | Plan-change boundary + rating | Boundary owner here, math owner rating; end-to-end alignment tested against the joint fixture | Baseline (workshop-pending) |
| Horizontal partitioning; 100K+ active subs/tenant | Aggregate store + read models | Partition by the pinned `orderingTenantId` (SUB-D-06 — stable across transfers, no row migration); bulk roll-up read models; batch Policy where contractually safe | Baseline (workshop-pending) |

#### Key ADRs

The load-bearing rationale is captured as ADRs (authored 2026-07-15, `status: proposed` / flagged
for veto — see [`ADR/`](./ADR/)):

| ADR | Decision captured |
|-----|-------------------|
| [`ADR/0001`](./ADR/0001-cpt-cf-bss-subscriptions-adr-manifest-closed-status-machine.md) `cpt-cf-bss-subscriptions-adr-manifest-closed-status-machine` | Why trials, billing-only pause, and scheduled intents are **attributes/postures/pending-intents on the closed manifest enum**, not new statuses (§6.1, SUB-D-01/03/05); the enum stays `draft`/`active`/`suspended`/`cancelled`/`archived`. |
| [`ADR/0002`](./ADR/0002-cpt-cf-bss-subscriptions-adr-when-not-math-split.md) `cpt-cf-bss-subscriptions-adr-when-not-math-split` | Why Subscriptions owns only the change **boundary/mode** and rating owns all proration math — the split that keeps replay deterministic (§6.3, SUB-R1). |
| [`ADR/0003`](./ADR/0003-cpt-cf-bss-subscriptions-adr-scheduled-intents-on-aggregate.md) `cpt-cf-bss-subscriptions-adr-scheduled-intents-on-aggregate` | Why scheduled cancel/resume/ramp intents live **on the aggregate** (visible to the renewal job, Billing, audit) rather than as portal-side automation (SUB-D-01/04). |

### 1.3 Architecture Layers

A shared **Lifecycle Foundation** ([`design/01-foundation-lifecycle.md`](./design/01-foundation-lifecycle.md))
owns the subscription aggregate, the closed status machine, the `TransitionRequest` envelope with
idempotency + ordering + the Policy/OSS gate, effective-dated composition, versioning, and the audit
store. Each capability slice is a **handler** that requests transitions **through** the Foundation
under those invariants — it owns its capability policy (renewal, entitlements, trials) but never the
commit/idempotency/gate mechanics. The numeric prefix follows PRD §6 decomposition and the
dependency order in the graph below.

| Slice | Title | PRD §6 | Seams owned |
|-------|-------|--------|-------------|
| [`design/01-foundation-lifecycle.md`](./design/01-foundation-lifecycle.md) | Lifecycle Foundation | §6.1 | SUB-E1, SUB-C4, SUB-N1 |
| [`design/02-composition-versioning.md`](./design/02-composition-versioning.md) | Composition & Versioning | §6.2 | SUB-R2, SUB-G2 |
| [`design/03-plan-changes.md`](./design/03-plan-changes.md) | Plan & Quantity Changes | §6.3 | SUB-R1, SUB-R3, SUB-P1, SUB-C2, SUB-G1, SUB-B6, **SUB-P7** (migration executor, §4.7) |
| [`design/04-suspension-renewal-grace.md`](./design/04-suspension-renewal-grace.md) | Suspension, Renewal & Grace | §6.4, §6.5 | SUB-B2, SUB-C1, SUB-F1, SUB-B5, SUB-E2, **SUB-P6** (expiry signal + notice lookahead, §4.3/§4.5) |
| [`design/05-entitlements.md`](./design/05-entitlements.md) | Entitlement Lifecycle | §6.9 | SUB-P2, SUB-E3, SUB-B4 |
| [`design/06-trials.md`](./design/06-trials.md) | Trial Runtime & Conversion | §6.10 | SUB-R4, SUB-P3 |
| [`design/07-tenancy-transfer.md`](./design/07-tenancy-transfer.md) | Multi-Tenant Ownership & Transfer | §6.6 | (AMS delegation; transfer approvals) |
| [`design/08-events-billing.md`](./design/08-events-billing.md) | Event Model & Billing Alignment | §6.7, §6.8 | SUB-B1, SUB-R1 (ordering), SUB-R6, SUB-C5, **SUB-B7** (cancellation money path), **SUB-B8** (one-time lane) |
| [`design/09-consumer-contracts.md`](./design/09-consumer-contracts.md) | Consumer & Integration Contracts | §9 | SUB-R1, SUB-B1, SUB-B7/B8 (wire payloads), SUB-C1, SUB-E1/E3, SUB-F1, SUB-G1, **SUB-P6** (read contract), **SUB-P8** (presence read, §4.1) |

*(Seam-map completion 2026-08-01, wave-3 review #18: SUB-P6/P7/P8/B7/B8 added to their owning slices — the SEAMS "each slice implements the seams listed for it here" contract had left the 2026-07-28…30 seams with no implementer; SUB-P9 stays parked on SEAMS pending the SB1 resolution.)*

#### Dependency order

```text
01-foundation-lifecycle (aggregate, status machine, TransitionRequest, idempotency/ordering, Policy/OSS gate, versioning, audit)
    │
    ├─→ 02-composition-versioning  (effective-dated PlanLink/AddOn, snapshot segment, PlanTier)
    │        │
    │        ▼
    ├─→ 03-plan-changes            (change boundary/mode, updateQuantity, ramps, overlap — needs composition)
    ├─→ 04-suspension-renewal-grace (suspend/resume, collectionPaused, renewal job, grace ladder)
    ├─→ 05-entitlements            (assignment from grant sets, check surface — needs composition + phases)
    ├─→ 06-trials                  (phase runtime + conversion — needs composition + entitlements)
    ├─→ 07-tenancy-transfer        (tenant axes, delegation, transfer approvals)
    ├─→ 08-events-billing          (producers, ordering, recurring idempotency — projects committed state)
    └─→ 09-consumer-contracts      (the boundary surface over all of the above)
```

- `02-composition-versioning` is the substrate the commercial slices read/write; it follows the Foundation.
- `03-plan-changes` needs effective-dated composition (the boundary opens/closes `PlanLink` intervals).
- `04`, `05`, `06` are parallel capabilities over the aggregate + composition; `06-trials` builds on `05-entitlements` (per-phase grant re-issue) and the phase machinery.
- `08-events-billing` projects committed state, so it follows the capabilities that produce it.
- `09-consumer-contracts` is the integration surface over the whole gear, so it is last.

## 2. Principles & Constraints

The lifecycle-wide normative statements are authored in the Foundation design; they are surfaced
here as design principles/constraints with stable ids.

### 2.1 Design Principles

#### Manifest-closed status machine

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-manifest-closed-enum`

`Subscription.status ∈ {draft, active, suspended, cancelled, archived}` is closed and terminal;
**trials, billing-only pause, and scheduled intents are attributes/postures/pending-intents on that
enum, never new statuses** (§6.1, SUB-D-03/05). A new status requires a manifest change first.

#### WHEN, not math

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-when-not-math`

Subscriptions owns the change **boundary and mode** (`changeEffectiveAt`, `changeMode`) and computes
**no** monetary charge, proration, tax, or FX. The rating gear owns the math; Billing owns posting.
One boundary owner + one math owner is the only split that keeps replay deterministic (§6.3).

#### Idempotent, ordered transitions

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-idempotent-ordered`

Every mutation is a `TransitionRequest` with exactly **one** durable effect per
`(subscriptionId, idempotencyKey)`; event and command order is preserved within
`(orderingTenantId, subscriptionId)` — the pinned-at-creation key shared with rating partition
ordering, immutable across ownership transfers (§6.7, SUB-D-06).

#### Fail-closed gate

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-fail-closed-gate`

Every **resource-affecting** transition passes the Policy Engine pre-check before commit; on **deny
or unavailability** the subscription state MUST NOT change (§6.1, SUB-E1). OSS resource topology is
mutated only via confirmed work orders — never directly.

#### Effective-dated, reproducible composition

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-effective-dated-composition`

Composition is a set of effective-dated intervals; history is never destructively edited. Posted
fee artifacts carry `pricingSnapshotRef` (Subscriptions writes the `(currency, region)` segment) so
Billing/rating reproduce a posted period from frozen inputs, never from live catalog state (§6.2).

### 2.2 Constraints

#### Not an authoring System of Record

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-not-authoring-sor`

Subscriptions resolves **published** catalog (`Plan`/`Price`/`PriceWindow`) and registry
(`skuId`/`PlanTier`/`CatalogVersion`) facts; it authors no catalog entity, evaluates no overlay, and
computes no charge. Draft or non-sellable catalog state fails the transition guard fail-closed
(SUB-P5, SUB-G2).

#### UTC time

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-utc-time`

All effective dating, change boundaries, term/renewal windows, grace and notice math, and scheduled
intents are UTC.

#### Tenant isolation; delegation-proofed cross-tenant

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-tenant-isolation`

Subscriptions are tenant-scoped on the three axes (`resourceTenantId`, `payerTenantId`,
`sellerTenantId`) referenced from AMS — never invented (§6.6). Any cross-tenant admin action carries
an auditable **delegation proof** or is rejected with the proof reference recorded (SUB-N/AMS).

#### Posted-document immutability

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-posted-immutability`

Posted invoice lines are never rewritten; subscription corrections flow as **new billable or
adjustment** artifacts (§6.8, SUB-B1). Subscription state and posted financial state are separate
datasets.

## 3. Technical Architecture

The technical architecture is specified per slice in the [`design/`](./design/) set, with the shared
substrate in [`design/01-foundation-lifecycle.md`](./design/01-foundation-lifecycle.md). This section
summarises the cross-slice shape and declares the component/sequence ids; the slice map and
dependency order are in §1.3.

### 3.1 Domain Model

_TBD (skeleton)._ The core aggregate — `Subscription` (status, `version`, tenant axes, activation
instants, composition, postures) with its child entities `PlanLink`, `AddOn`, `TransitionRequest`,
`Approval`, `Entitlement`, and the scheduled-intent / renewal-evaluation value objects — is owned by
`01-foundation-lifecycle`; per-capability shapes by their slices. Catalog and registry entities are
**resolved, frozen inputs**, not owned here. Full field-level definitions and the naming discipline
are normative in the Foundation slice §3.1.

### 3.2 Component Model

Components are handlers over the shared Foundation, not independently deployable services. Each
carries a stable `cpt-cf-bss-subscriptions-component-{slug}` ID; the linked slice doc is normative
for its internals.

#### Lifecycle Foundation

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-lifecycle-foundation`

Subscription aggregate, closed status machine + guards, `TransitionRequest` envelope with
idempotency + `(orderingTenantId, subscriptionId)` ordering (the tenant pinned at creation, SUB-D-06), the Policy/OSS fail-closed gate,
versioning, activation instants, and the audit store
([`design/01-foundation-lifecycle.md`](./design/01-foundation-lifecycle.md)).

#### Composition & Versioning handler

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-composition-versioning`

Effective-dated `PlanLink`/`AddOn` intervals, monotonic `version`, the `(currency, region)`
`pricingSnapshotRef` segment, `PlanTier` derivability, per-sale brand attribution
([`design/02-composition-versioning.md`](./design/02-composition-versioning.md)).

#### Plan & Quantity Changes handler

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-plan-changes`

Change boundary/mode, up/down asymmetry, `updateQuantity` seat provenance, ramp execution of
scheduled intents, overlap-cardinality detection, backdating guards
([`design/03-plan-changes.md`](./design/03-plan-changes.md)).

#### Suspension, Renewal & Grace handler

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-suspension-renewal-grace`

Suspend/resume, `collectionPaused` posture, the renewal job (auto/manual), notices + opt-out, the
failed-renewal grace ladder with evaluated fields
([`design/04-suspension-renewal-grace.md`](./design/04-suspension-renewal-grace.md)).

#### Entitlement Lifecycle handler

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-entitlements`

Issue/revoke from transitions, assignment from the pricing grant set (incl. per-phase), the
point-of-use check decision state (p95 < 100ms), quota soft/hard limits
([`design/05-entitlements.md`](./design/05-entitlements.md)).

#### Trial Runtime & Conversion handler

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-trials`

Trial provisioning on the phase machinery, end-of-trial + early `convertTrial`, expiry, extension
([`design/06-trials.md`](./design/06-trials.md)).

#### Multi-Tenant Ownership & Transfer handler

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-tenancy-transfer`

Tenant-axis resolution from AMS, delegation-proof enforcement, hierarchy by reference, and the
approval-gated ownership transfer flow
([`design/07-tenancy-transfer.md`](./design/07-tenancy-transfer.md)).

#### Event Model & Billing Alignment handler

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-events-billing`

The producer inventory + payload-sufficiency rules, ordering, recurring `BillableItem` idempotency,
traceability, and the event outbox
([`design/08-events-billing.md`](./design/08-events-billing.md)).

#### Consumer & Integration Contracts handler

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-consumer-contracts`

The integration surface: Billing handoff, rating read-model, Contracts input, Policy gate, OSS
provisioning, Payments signals ([`design/09-consumer-contracts.md`](./design/09-consumer-contracts.md)).

### 3.3 API Contracts

_TBD (skeleton)._ The **control-plane operations** contract
(`cpt-cf-bss-subscriptions-interface-control-plane`, §9.1) and the external integration contracts
(`-contract-billing-handoff`, `-contract-rating-read-model`, `-contract-contracts-input`,
`-contract-policy-gate`, `-contract-oss-provisioning`, `-contract-payments-signals`, §9.2) are owned
by `09-consumer-contracts` and PRD §9; the point-of-use entitlement **check** read contract is owned
by `05-entitlements`. Concrete REST mappings, headers, OpenAPI, event field matrices, and error
taxonomies are owned by the slice designs, not this index (§6 content boundary).

### 3.4 Internal Dependencies

- **`toolkit-db`** — transactional persistence for the aggregate + revision history, the projected entitlement-check read model, the audit store, and the event outbox.
- **Coordination lease library** — singleton coordination for background work (renewal job, notice dispatch, scheduled-intent execution, recurring-generation cut).

### 3.5 External Dependencies

Integration boundaries, not components owned here (PRD §3.2 / §13):

- **Pricing (Product Catalog)** — published `Plan`/`Price`/`PriceWindow` linkage, plan-change classification, phase → grant-set map, trial offers, sellability gate (SUB-P*).
- **Rating (evaluation core + pipeline)** — consumes composition read models + `(changeEffectiveAt, changeMode)`; owns proration math + usage slicing; shared ordering key (SUB-R*).
- **Catalog registry (Product & SKU)** — published `skuId`/`PlanTier`/`CatalogVersion`, `catalogSubscriptionProductKey` for the overlap key; upstream PR #4177, not vendored (SUB-G*).
- **Contracts & Agreements** — renewal/grace/regional templates, ramps, commitment pools, booking date, `PriceOverride` windows (SUB-C*).
- **Billing & Invoicing** — recurring ingestion, posting, adjustments, floor/cap + rounding, dunning, `collectionPaused` artifact treatment (SUB-B*).
- **Policy Engine / OSS Provisioning** — fail-closed gate; provisioning confirmation; entitlement enforcement execution (SUB-E*).
- **Payments (PSP)** — pre-check + retry-exhaustion signals; authorization at renewal/conversion (SUB-F1). **Notifications/Comms** — notice/win-back delivery (SUB-F2).
- **AMS / OSS (tenant identity)** — tenant identity + topology, delegation-proof backbone.

### 3.6 Interactions & Sequences

Per-flow sequences are specified in the corresponding slice documents; the load-bearing ones:

#### Policy-gated transition

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-seq-policy-gated-transition`

Validate + create `TransitionRequest` → Policy pre-check (deny ⇒ no state change, `reasonCodes`) →
OSS provision/deprovision confirm where required → commit + increment `version` + issue/revoke
entitlements → emit ordered lifecycle events; idempotent on `(subscriptionId, idempotencyKey)`
([`design/01-foundation-lifecycle.md`](./design/01-foundation-lifecycle.md)).

#### Plan/quantity change boundary

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-seq-change-boundary`

Set `changeEffectiveAt`/`changeMode` (up/down asymmetry) → close prior / open new `PlanLink` interval
or schedule a boundary → emit `SubscriptionPlanChanged` carrying the boundary inputs → rating slices
usage + prorates at the same instant; no posted-invoice mutation
([`design/03-plan-changes.md`](./design/03-plan-changes.md)).

#### Renewal + grace ladder

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-seq-renewal-grace`

Renewal job at term end → **pending non-renewal intent?** ⇒ the intent fires instead (no extension)
→ **`autoRenew = false`, no manual `renew`?** ⇒ system end-of-term `cancel` (`term_expired`,
SUB-D-13 §4.3a) → otherwise payment pre-check → success: extend term + eligibility-first snapshot
re-resolution (SUB-D-14 as amended — a grandfathered subscription keeps its pinned generation
until `grandfatherUntil` passes; the notice job armed the 30-day commercial notice ahead where the
SUB-D-17 lookahead saw a pending change); failure:
enter grace (default 7 days, paused next-term recurring), notices, hybrid exit → `suspended`/
`cancelled` per Contract ladder; keyed to prevent double term extension. From nonpayment-`suspended`:
payment inside the SUB-D-16 dwell **revives** — backdated term extension per §4.3b with SUB-D-20
as-of resolution (composition/payer/eligibility as of what the backdated term describes) → `resume`;
past the (hold-adjusted) dwell deadline the daily sweep fires the terminal `cancel`
(`nonpayment_exhausted`, SUB-D-22)
([`design/04-suspension-renewal-grace.md`](./design/04-suspension-renewal-grace.md)).

#### Point-of-use entitlement check

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-seq-entitlement-check`

A tenant-isolated read of the projected check surface (feature flag, quota remaining, limit state) at
p95 < 100ms; OSS enforces the decision; updates propagate to the surface within the §7.1 baseline
([`design/05-entitlements.md`](./design/05-entitlements.md)).

#### Trial conversion

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-seq-trial-conversion`

Advance the phase boundary (`convertsToPhaseId` at term, or `convertTrial` to `now`) → authorize
payment where required → re-issue entitlements per the target phase's grant set with no access gap →
emit the composition-changing event; idempotent (zero missed / zero double conversions)
([`design/06-trials.md`](./design/06-trials.md)).

### 3.7 Database schemas & tables

The canonical schema — the `subscription` aggregate + append-only revision history, `plan_link` /
`add_on` interval tables, `transition_request` (with idempotency key) + `approval`, the
`entitlement` state + the projected **entitlement-check read model**, scheduled-intent and
renewal-evaluation tables, the event outbox, and the append-only audit store — is owned by the
Foundation and specified normatively in
[`design/01-foundation-lifecycle.md`](./design/01-foundation-lifecycle.md) §3.7. Slice-specific
tables (grace evaluation, trial phase state, transfer approvals) are introduced by their slice
documents. All stores are tenant-partitioned; effective dating is UTC.

### 3.8 Deployment Topology

The subscriptions gear runs as a stateful control-plane + read-model service over a shared
`toolkit-db` backend, partitioned by the pinned `orderingTenantId` (SUB-D-06 — stable across
transfers, so no aggregate row set migrates partitions). Background work (renewal job, notice
dispatch, scheduled-intent execution, recurring-generation cut) is coordinated as a singleton via the
coordination lease library. The entitlement-check read path is served from the projected read model
for the p95 < 100ms target. Deployment specifics are platform-standard for a BSS gear.

## 4. Additional context

**Cross-cutting normatives** (frozen resolutions from [`SEAMS.md`](./SEAMS.md), binding every slice):

- **WHEN/MATH split (SUB-R1):** Subscriptions owns `changeEffectiveAt`/`changeMode`; rating owns proration math + usage slicing; shared ordering key `(orderingTenantId, subscriptionId)` — the ordering tenant is **pinned at creation** and immutable across transfers (SUB-D-06).
- **Recurring split (SUB-D-07, SUB-R6):** Subscriptions cuts the money-free recurring **period fact** (anchor, pauses, intents, the idempotency key); rating **prices** it; Billing posts. No monetary column in this gear.
- **Snapshot segment (SUB-R2):** Subscriptions writes only the `(currency, region)` segment of `pricingSnapshotRef` at activation; seat provenance and the activation date-trio are **not** snapshot segments — they ride events/read-models.
- **Adopt the catalog contracts (SUB-P1/P2/P3/P5):** plan-change classification (boundary class computed here since pricing D-93), phase → grant-set map, trial offer, and the sellability gate (incl. pricing D-80's coverage horizon + D-94's full-conjunction granularity — slice 01 §4.5) are pricing/catalog-authored and adopted; Subscriptions authors nothing catalog-side.
- **Joint pricing lanes (SUB-P6/P7/P8, added 2026-07-28…30; slice owners assigned 2026-08-01):** the grandfathering expiry signal + notice lookahead + as-of reads (slices 04/09), the plan-migration executor handshake (slice 03 §4.7), and the in-flight-subscriber presence read (slice 09 §4.1). `billingAnchorPolicy` adoption (SUB-P9) is parked on the SB1 recurring-WHEN resolution.
- **Contracts obligation (SUB-C1):** renewal/grace/regional templates are Contracts SoR; until the upstream Contracts PRD authors them, the platform defaults govern (7-day grace, 30/14/7/1 notices).
- **Billing invariants (SUB-B1/B7/B8):** recurring idempotency `(subscriptionId, billing period, lineKey)` (per billable component interval — SUB-D-19/21), posted-invoice immutability, **per-component** `{skuId, planId, priceId}` + `pricingSnapshotRef` traceability; every cut resolves **as of what it describes** (SUB-D-20); the one-time lane emits amount-less and Billing values from the frozen ref (SUB-D-24); ETF derivation is reason-scoped with a term/period join key (SUB-D-25); Billing exposes the `billedThroughAt` watermark for the backdating guard (SUB-B6). The recurring-WHEN owner vs rating's period tick is the open SB1 CRIT (SUB-B1).
- **Fail-closed gate + enforcement split (SUB-E1/E3):** Policy gates resource-affecting transitions fail-closed; Subscriptions serves the entitlement decision state, OSS enforces; the check **read** surface follows the bounded-staleness degraded mode (SUB-D-10), transitions never do.
- **Closed manifest enum (SUB-D-03/05/11):** trials, billing-only pause, and scheduled intents are attributes/postures/pending-intents, never new statuses; `draft → cancelled` (void) completes the edge set so drafts are exitable.
- **One commit path, complete inventories (SUB-D-08/09):** every FR-mandated mutation has a `TransitionRequest.type`; every FR-mandated auditable event has a name in the slice-08 registry.

**Autonomous decisions** — [`DECISIONS.md`](./DECISIONS.md) SUB-D-01…26 (**ALL CONFIRMED per-item 2026-08-01** — the gear's first veto round; joint qualifiers per SEAMS stay owed):
scheduled intents, `updateQuantity`, `collectionPaused`, ramps, activation date-trio, pinned
ordering tenant, recurring split, mutation-type completion, event-inventory naming, check-surface
staleness budget, draft void, pause × renewal interplay (01…12); term-boundary resolution,
eligibility-first re-resolution, mid-ramp supersession, nonpayment dwell, price-change notice,
cancellation money path, per-component recurring key (13…19); as-of cut resolution, `lineKey`
identity + emitter completeness, dwell lifecycle, parked firing class, one-time lane, ETF reason
class, cancel+new cohort disclosure (20…26).

**Open cross-team items (Design-lock inputs)** — the highest-risk is the **Contracts** renewal/grace
SoR (SUB-C1, upstream unauthored); then the rating read-model **field mirror** (SUB-R1, downgraded
from ALIGNED — seat quantity @ `t`, `priceEligibility` inputs), the recurring-pricing counterpart +
joint fixture (SUB-R6), the brand-context source discrepancy (SUB-R5), the rating seat-boundary
transport (SUB-R3), the OSS enforcement + quota mid-request contract and staleness-budget default
(SUB-E3, SUB-D-10), the Billing pause/prepaid lines + `billedThroughAt` watermark (SUB-B2/B4/B6),
the registry `catalogSubscriptionProductKey` shape (SUB-G1, PR #4177), and the
acceptance-confirmation flow (SUB-C4). Manifest alignment for the new `TransitionRequest.type`
values + intent envelope (SUB-N1) is tracked in [`PRD.md`](./PRD.md) §15, not a design blocker.

## 5. Traceability

- **PRD**: [`PRD.md`](./PRD.md) (§6 functional; §7 NFR; §9 contracts; §12 acceptance criteria; §15 open questions).
- **Cross-gear contract**: [`SEAMS.md`](./SEAMS.md) (seam register organised by neighbour + decision log).
- **Decisions**: [`DECISIONS.md`](./DECISIONS.md) (SUB-D-01…26, flagged for veto).
- **ADRs**: [`ADR/`](./ADR/) — [`0001`](./ADR/0001-cpt-cf-bss-subscriptions-adr-manifest-closed-status-machine.md) (manifest-closed status machine), [`0002`](./ADR/0002-cpt-cf-bss-subscriptions-adr-when-not-math-split.md) (WHEN/MATH split), [`0003`](./ADR/0003-cpt-cf-bss-subscriptions-adr-scheduled-intents-on-aggregate.md) (scheduled intents on the aggregate) — all `status: proposed`, flagged for veto.
- **Slices**: [`design/`](./design/) (per-slice designs) and [`design/README.md`](./design/README.md).
