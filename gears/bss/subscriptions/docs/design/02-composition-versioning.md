Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Subscriptions — Composition & Versioning (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Pricing (published Plan/Price/PriceWindow), Registry (skuId/PlanTier/CatalogVersion) | Downstream: Rating (composition read-model), Billing (snapshot refs) | Owners: BSS Subscriptions team -->

# DESIGN — Composition & Versioning (Slice 2)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-design-composition`

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
  - [4.1 Effective-Dated Intervals (normative)](#41-effective-dated-intervals-normative)
  - [4.2 Snapshot Segment and Reproducibility (normative)](#42-snapshot-segment-and-reproducibility-normative)
  - [4.3 PlanTier Derivability (normative)](#43-plantier-derivability-normative)
  - [4.4 Per-Sale Brand Attribution (normative)](#44-per-sale-brand-attribution-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice owns **what applied when**: the effective-dated composition (`PlanLink`/`AddOn`
intervals), the monotonic `version` lineage, and the snapshot discipline that lets rating and
Billing agree on the offer in force for any interval. Composition is **interval algebra over frozen
catalog references** — Subscriptions never edits history destructively; a change opens a new interval
and closes the prior one ([`../PRD.md`](../PRD.md) §6.2). The slice runs through the Foundation
commit path (slice 01); it adds no side door.

Its cross-gear surface is two seams: **SUB-R2** (Subscriptions writes only the `(currency, region)`
segment of the composed `pricingSnapshotRef` at activation; rating is the composition SoR that seals
the ref) and **SUB-G2** (effective `PlanTier` is derivable @ `t` from the published registry facts,
read-only).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-subscriptions-fr-effective-dated-composition` | `PlanLink (subscriptionId, planId, effectiveFrom, effectiveTo)` + `AddOn (startDate, endDate)` as **half-open UTC intervals**; a change opens/closes intervals, never rewrites them (§4.1). |
| `cpt-cf-bss-subscriptions-fr-monotonic-version` | Each commercial-meaning change increments `version` and appends an immutable `SubscriptionRevision` (mechanic in slice 01 §3.7); composition changes are the primary version driver (§4.1). |
| `cpt-cf-bss-subscriptions-fr-snapshot-discipline` | Fee artifacts carry `pricingSnapshotRef`; Subscriptions freezes the `(currency, region)` binding at activation into the composed ref so Billing never re-resolves mutable catalog for posted periods (§4.2). |
| `cpt-cf-bss-subscriptions-fr-plantier-derivability` | Effective `PlanTier` is a pure function of SKU/Plan @ event time; changes are effective-dated + Policy-gated (§4.3). |
| `cpt-cf-bss-subscriptions-fr-sale-brand-attribution` | The per-sale `brandId` is a Subscriptions attribute published in the evaluation context as a brand-context candidate for overlay matching in rating — the authoritative source is the **open** SUB-R5 seam (§4.4). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-subscriptions-nfr-operational-baselines` | Interval store + read projections | Composition read (effective `PlanLink`/`AddOn` @ `t`) p95 < 200ms via an indexed interval query | Load test; baseline (workshop-pending [`../PRD.md`](../PRD.md) §7.1) |
| `cpt-cf-bss-subscriptions-nfr-horizontal-partitioning` | Interval store | Tenant-partitioned; bulk roll-up read models for account views | Design + load test |

#### Key ADRs

No slice-local ADR; the snapshot split is governed by SEAMS **SUB-R2** (rating owns the composed ref;
Subscriptions writes one segment) and the versioning mechanic by slice 01.

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-subscriptions-tech-stack-cmp`

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | Composition mutation (open/close intervals) via the Foundation commit path; snapshot-segment stamping at activation | Rust module in the `subscriptions` gear |
| Domain | `PlanLink`/`AddOn` interval value objects, `PlanTier` resolution, `pricingSnapshotRef` segment | Rust; GTS + Rust domain structs |
| Infrastructure | Interval tables + projected composition read model | PostgreSQL, SecureORM |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Composition is append-only interval algebra

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-append-only-intervals-cmp`

A composition change **opens a new interval and closes the prior** at the boundary; historical
intervals are immutable. Rating and Billing read the interval in force @ `t` ([`../PRD.md`](../PRD.md)
§6.2).

#### One segment, not the whole ref

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-one-segment-cmp`

Subscriptions writes **only** the `(currency, region)` binding into the composed `pricingSnapshotRef`;
the ref is rating's composition SoR. Subscriptions never mints or seals the whole ref (SEAMS
**SUB-R2**).

### 2.2 Constraints

#### Frozen catalog references only

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-frozen-refs-cmp`

`PlanLink`/`AddOn` reference **published** catalog keys resolved at the boundary; a posted period
reproduces from the frozen ref, never from live catalog state ([`../PRD.md`](../PRD.md) §6.2, §6.8).

#### PlanTier is derived, never authored

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-plantier-derived-cmp`

Effective `PlanTier` is derived from the registry-published SKU/Plan; this gear neither authors nor
overrides the taxonomy (SEAMS **SUB-G2**).

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-domain-model-cmp`

- **`PlanLink`** — `(subscriptionId, planId, effectiveFrom, effectiveTo)`, half-open UTC; the interval a plan applies for rating/billing.
- **`AddOn`** — an optional add-on with `startDate`/`endDate`; same interval discipline.
- **`pricingSnapshotRef` segment** — the Subscriptions-written `(currency, region)` binding frozen at activation, contributed to the rating-sealed composed ref (SEAMS **SUB-R2**; [rating PRD](../../../rating/docs/PRD.md) §1.4).
- **`PlanTierResolution`** — the effective `PlanTier` @ `t`, derived from the published SKU/Plan.
- **`brandId`** — per-sale storefront brand attribute (§4.4).

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-composition-cmp`

- **`CompositionManager`** — opens/closes `PlanLink`/`AddOn` intervals through the Foundation commit path; guarantees no interval overlap or destructive edit.
- **`SnapshotSegmentStamper`** — freezes the `(currency, region)` binding at activation and contributes the segment to the composed ref.
- **`PlanTierResolver`** — resolves effective `PlanTier` @ `t` from published catalog/registry facts.

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-interface-composition-cmp`

The composition **read-model** (effective `PlanLink`/`AddOn` intervals, `PlanTier` @ `t`, active
phase @ `t`) is the rating input contract; its shape is owned by
[`09-consumer-contracts.md`](./09-consumer-contracts.md) (SUB-R1). This slice fixes the interval
semantics and the snapshot segment; wire mappings are Design.

### 3.4 Internal Dependencies

Depends on [`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) (commit path, versioning);
feeds [`03-plan-changes.md`](./03-plan-changes.md) (the boundary opens/closes intervals),
[`06-trials.md`](./06-trials.md) (phase intervals), and [`08-events-billing.md`](./08-events-billing.md)
(composition-changing events).

### 3.5 External Dependencies

| Dependency | What crosses the boundary | Contract |
|------------|---------------------------|----------|
| Pricing | Published `planId`/`PriceWindow` linkage `PlanLink` resolves against | [`../PRD.md`](../PRD.md) §9; SEAMS **SUB-P5** |
| Registry | Published `skuId`/`PlanTier` taxonomy | SEAMS **SUB-G2** |
| Rating | Consumes the composition read model + seals the composed `pricingSnapshotRef` | SEAMS **SUB-R2**, **SUB-R1** |

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-flow-open-interval-cmp`

**Composition change**: at the change boundary (slice 03), `CompositionManager` closes the prior
`PlanLink`/`AddOn` interval `effectiveTo = boundary` and opens the new one `effectiveFrom = boundary`
in one Foundation commit; `version` increments; a composition-changing event is emitted (slice 08).
At activation, `SnapshotSegmentStamper` freezes the `(currency, region)` segment.

### 3.7 Database Schemas and Tables

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-storage-composition-cmp`

Owned here: `plan_link` and `add_on` interval tables (tenant-partitioned by the pinned
`orderingTenantId`, UTC), plus the `quantity_interval` history (the effective-dated committed seat
count, written by slice 03) and the projected `composition_read_model`. **No-overlap exclusion keys
(2026-07-15 review fix):** `plan_link` per `(subscriptionId)` — one plan in force at a time;
`add_on` per `(subscriptionId, addOnId)` — concurrent *different* add-ons legitimately overlap in
time, only the same add-on may not self-overlap; `quantity_interval` per `(subscriptionId)`. The
`(currency, region)` segment rides the aggregate; the composed ref lives in the fee artifact
(rating/Billing). Concrete DDL is Design.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-subscriptions-deployment-cmp`

No slice-specific topology beyond the Foundation's; the composition read model is a projection served
off the commit path ([`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) §3.8).

## 4. Additional Context

### 4.1 Effective-Dated Intervals (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-intervals-cmp`

- `PlanLink`/`AddOn` are **half-open UTC intervals**; a change closes the prior interval at the boundary and opens a new one — never a destructive edit of history ([`../PRD.md`](../PRD.md) §6.2). **Interval ordinal (SUB-D-21, 2026-08-01):** each component's intervals carry a **1-based lifetime ordinal** assigned at interval open, immutable — it is the occurrence dimension of the billing `lineKey` (`plan#n` / `addon:{addOnId}#n`, slice 08 §3.1), so an add-on removed and re-added within one period yields two intervals, two ordinals, and two period facts instead of colliding on one key.
- **Past is immutable; future is replaceable.** Intervals whose `effectiveFrom` has not been reached (a scheduled next-cycle/end-of-term change writes them ahead) MAY be voided or replaced by the owning `unschedule`/superseding change — **or by the slice 01 §4.3 terminal sweep, which counts as an owning superseding change** (2026-08-01 wave-3 review #14: an immediate cancel must not leave an un-reached `PlanLink` interval to open on a cancelled subscription; the sweep also closes in-force intervals at the terminal instant) — until they take effect; that is an edit of the *future*, evented and audited, never of history. Once `effectiveFrom` passes, the interval is history and immutable.
- Rating and Billing resolve the interval **in force @ `t`**; the boundary instant is the one the change owner (slice 03) sets, shared with rating slicing (SUB-R1). The same @-`t` discipline covers the **committed quantity** (`quantity_interval`, slice 03) — `quantity @ t` is part of the rating read model (PRD §9.2).
- Each committed composition change increments `version` (slice 01 mechanic) for optimistic concurrency + audit lineage.

### 4.2 Snapshot Segment and Reproducibility (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-snapshot-segment-cmp`

- Subscriptions freezes the `(currency, region)` binding at **activation** and contributes it as **one segment** of the composed `pricingSnapshotRef`; rating is the composition SoR that seals the ref (SEAMS **SUB-R2**; [rating PRD](../../../rating/docs/PRD.md) §1.4). The segment **persists across renewals** (any renewal's snapshot-ref re-resolution — auto and manual alike, SUB-D-14 as amended — touches the pricing-side segments only, eligibility-first: `priceEligibility`/`cohort` carry forward for grandfathered subscriptions, slice 04 §4.3) and across ownership transfers; only **cancel+new** re-freezes it — which is exactly why cross-currency/region changes are cancel+new (slice 03 §4.3).
- Seat-count provenance (slice 03) and the activation date-trio (slice 01 §4.4) are **not** snapshot segments — they ride events/read-models. No fourth Subscriptions segment appears without a co-decision with rating.
- Posted fee artifacts reproduce from the frozen ref; a posted period never re-resolves live catalog ([`../PRD.md`](../PRD.md) §6.8).

### 4.3 PlanTier Derivability (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-plantier-cmp`

- Effective `PlanTier` MUST be derivable from the published SKU/Plan @ event time; a non-derivable tier fails the guard fail-closed (rating resolves tier-dependent pricing @ `t`) ([`../PRD.md`](../PRD.md) §6.2).
- `PlanTier` changes are effective-dated + Policy-gated; the taxonomy itself is registry-owned (SEAMS **SUB-G2**).

### 4.4 Per-Sale Brand Attribution (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-normative-brand-cmp`

- A subscription created under a storefront brand records the per-sale `brandId` and publishes it in the pricing evaluation context as **a** brand-context candidate for step-4 overlay matching ([rating PRD](../../../rating/docs/PRD.md) §17.1 step 4).
- **Which source actually feeds brand matching is an OPEN contested seam — SUB-R5** (2026-07-28 review fix, REVIEW **F-08-2**): this gear publishes the **per-sale** `brandId`, while rating matches `brand` against **Plan/SKU `brandId` @ `t`**. The two disagree, and **AC 20 is not implementable until they are reconciled** ([`../SEAMS.md`](../SEAMS.md) SUB-R5; [`../PRD.md`](../PRD.md) §15). This slice previously asserted the per-sale source as settled ("so rating matches…"), taking a side on an unresolved seam; it does not. Downstream consequences stay open with it: `brandId` behaviour under ownership transfer (REVIEW F-02-3 / F-07-2, slice [`07`](./07-tenancy-transfer.md) §4.4 defines no rebind).
- The registry `Product` declares brand *membership*; the **per-sale** `brandId` is a Subscriptions attribute — the one piece of brand context only this gear can supply ([`../PRD.md`](../PRD.md) §6.2). That makes it a *candidate* source, not the decided one.

## 5. Traceability

- **PRD**: [`../PRD.md`](../PRD.md) §6.2 (`fr-effective-dated-composition`, `fr-monotonic-version`, `fr-snapshot-discipline`, `fr-plantier-derivability`, `fr-sale-brand-attribution`), §6.8 (reproducibility), §7.1 (NFRs).

**Traces to**: `cpt-cf-bss-subscriptions-fr-effective-dated-composition`, `cpt-cf-bss-subscriptions-fr-snapshot-discipline`, `cpt-cf-bss-subscriptions-fr-plantier-derivability`, `cpt-cf-bss-subscriptions-fr-sale-brand-attribution` *(single-owner FR claims — the P2 traceability convention adopted 2026-08-01, wave-3 review #24h; shared mechanics other slices cite stay narrative, each FR has exactly one owning slice)*
- **Seams**: **SUB-R2** (snapshot segment), **SUB-G2** (PlanTier derivability); feeds **SUB-R1** (composition read model) — [`../SEAMS.md`](../SEAMS.md).
- **Slices**: [`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) (commit + version), [`03-plan-changes.md`](./03-plan-changes.md) (boundary), [`06-trials.md`](./06-trials.md) (phase intervals), [`08-events-billing.md`](./08-events-billing.md) (events), [`09-consumer-contracts.md`](./09-consumer-contracts.md) (read-model contract).
