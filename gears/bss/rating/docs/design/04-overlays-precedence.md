Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Overlays & Precedence (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Pricing (Product Catalog), Contracts & Agreements, OSS/AMS | Downstream: Rating | Owners: BSS Rating team -->

# DESIGN — Overlays & Precedence (Slice 4)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-design-overlays-precedence`

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
  - [4.1 Scope → Tenant-Axis Mapping (normative)](#41-scope--tenant-axis-mapping-normative)
  - [4.2 Stacking and the Total Order (normative)](#42-stacking-and-the-total-order-normative)
  - [4.3 Contract Overlay Precedence (normative)](#43-contract-overlay-precedence-normative)
  - [4.4 Bounded Composition — Anti-Drift Cap (normative)](#44-bounded-composition--anti-drift-cap-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

Overlays & Precedence is the **steps 4–5 evaluator**: it resolves which `PriceOverlays`
survive the scope filter for the evaluation context, applies **all** survivors as a sequential
**stack** in one deterministic total order, then applies the customer/contract overlay on top —
`Contract > Partner price overlays > Catalog base`. The stacking decision is the load-bearing seam
resolution of this slice (SEAMS **O3, resolved 2026-07-10**): step 4 **stacks** — partner +
brand + region overlays are legitimately cumulative; the pricing **class-specificity order**
`customerGroup > partner > orgTier > brand > region > global` (pricing `design/09`, adopted
verbatim — O1, including the `customerGroup` class Rating was missing — O2) is the
**cross-class tie-break inside the stack**, never a winner-take-all filter.

Determinism comes from three locks: the scope→tenant-axis mapping is a fixed table (no
heuristic tenant matching — `resourceTenantId` alone never matches a partner list); the stack
order is total (`precedence` → class order → `priceOverlayId`); and the cumulative result is
bounded by the **anti-drift cap** (`maxCumulativeMarkup`) — clamp-and-record or hard-fail,
never silent unbounded compounding. The applied overlay set is a Rating-written segment of
`pricingSnapshotRef` (SEAMS S1), and per-layer lineage rides the outcome for audit. Overlay
*authoring* and publish-time precedence validation live in the pricing gear; contract-overlay
authoring lives in Contracts; this slice only evaluates ([`../PRD.md`](../PRD.md) §6.4).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-fr-priceoverlay-scope-mapping` | `ScopeFilter` implements the fixed scope→axis table (§4.1): `global` always; `customerGroup` by the payer's BSS-resolved group at `t`; `partner`/`orgTier` by `sellerTenantId`; `brand` by Plan/SKU `brandId` at `t`; `region` by the usage/price-row region key; `resourceTenantId` never qualifies a partner/orgTier list alone. |
| `cpt-cf-bss-rating-fr-overlay-stacking` | `OverlayStacker` applies **all** survivors sequentially in the total order: ascending `precedence` → class-specificity order for cross-class ties → ascending `priceOverlayId` within class (§4.2); the class order breaks ties, it does not pick a single winner (O3). |
| `cpt-cf-bss-rating-fr-customer-contract-overlay` | `ContractOverlayApplier` applies contract/account overrides **after** step 4; contract terms outrank partner lists; dimension-integrity and approval bounds are publish-side guarantees this evaluator relies on (§4.3). |
| `cpt-cf-bss-rating-fr-bounded-composition-cap` | `CompositionCapGuard` tracks the cumulative markup/discount across the partner → reseller → customer chain against `maxCumulativeMarkup`: clamp-and-record, or fail-closed when the cap is hard (§4.4). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-throughput-latency` | `ScopeFilter` + `OverlayStacker` | Filtering and stacking are in-memory over pinned `PriceOverlay` pages; the stack length is bounded by the published overlay set; no I/O in steps 4–5 | Load test; targets provisional (NFR workshop) |
| `cpt-cf-bss-rating-nfr-audit-segregation` | Lineage + publish-side controls | Per-layer pre/post amounts + applied ids ride the outcome (01 §4.4 lineage); customer-layer changes cannot weaken audit controls — Contract workflow + optional Finance approval are upstream obligations this slice records, never relaxes | Audit fixture |
| `cpt-cf-bss-rating-nfr-resilience` | `CompositionCapGuard` + runtime safety net | Publish-time validation rejects equal-precedence overlap within a class; the runtime total order still yields a single deterministic result if a defective catalog slips through (never an arbitrary pick) | Chaos test + joint fixtures |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-bss-rating-adr-scope-key-adoption` | The `priceOverlay` axis of the 8-axis key carries the base row; overlay lists are step-4 stack material — the split this slice implements (SEAMS K1/O1–O3). |
| `cpt-cf-bss-pricing-adr-canonical-scope-key` (adopted) | Key definition SoR; `priceOverlay` is column 4. |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-ovl`

```text
Steps 4–5 evaluator (this slice)  ScopeFilter · OverlayStacker · ContractOverlayApplier ·
        │  (registers into §17.1 slots 4–5)   CompositionCapGuard · OverlayLineageRecorder
        ▼
Foundation mechanisms (01)        EvaluationPipeline · SnapshotComposer (overlay segment) ·
                                  MetadataRecorder (per-layer lineage)
        │
        ▼
Frozen inputs                     pinned PriceOverlay set (pricing) · contract overlay terms
                                  (Contracts) · payer customerGroup at t (BSS claims, frozen in ctx)
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | The steps 4–5 evaluator: scope filtering, stacking, contract overlay, cap guard | Rust module in the `rating` gear (rating-core crate) |
| Domain | Scope classes, stack order key, overlay lineage, cap policy shapes | Rust; GTS + Rust domain structs |
| Infrastructure | **None owned** — overlay definitions arrive pinned; contract terms arrive frozen | In-process (01 §3.7) |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Stack all survivors; tie-break, don't exclude

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-stack-not-winner-ovl`

Step 4 applies **every** scope-matching survivor — cumulative partner + brand + region economics
are the intended commercial behavior (SEAMS O3). The class-specificity order resolves *ordering
ambiguity between classes*; treating it as overlay exclusivity (most-specific-wins over the
whole stack) is the misreading this principle forbids.

#### One total order, no arbitrary picks

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-total-order-ovl`

The stack order is total and deterministic: ascending `precedence`, then the class order for
cross-class ties, then ascending `priceOverlayId` within a class as the final stable tie-break.
Every evaluation of the same frozen context stacks in byte-identical order (01 §4.2).

#### Axes are typed, not inferred

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-typed-axes-ovl`

Scope matching uses exactly the mapped tenant axis per §4.1 — a list never matches "because the
tenant looks related". `resourceTenantId` (usage tenancy) alone never qualifies partner/orgTier
lists; `payerTenantId`/`accountId` act only in step 5. Cross-tenant price leakage is the failure
this principle prevents ([`../PRD.md`](../PRD.md) §6.3).

### 2.2 Constraints

#### Publish-side validation is relied on, not re-run

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-publish-side-guarantees-ovl`

Equal `precedence` among lists with overlapping scope **within one class** is rejected at
publish (fail-closed, pricing-side pipeline — validator registration is slice
[`10`](./10-governance-asc606.md)'s G1 material); contract overlays introducing metering
dimensions absent from the published Plan/SKU revision are rejected at contract publish. The
runtime total order is a **safety net**, not a substitute: a defective catalog still yields a
single deterministic result, flagged in metadata.

#### The overlay segment is sealed

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-overlay-segment-ovl`

The resolved overlay / `priceOverlay` ids are a **Rating-written segment** of
`pricingSnapshotRef` at evaluation (SEAMS S1, 01 §4.3); re-resolution replays the same overlay
set from the pin — never a live re-filter against the current catalog (SEAMS W2).

#### Caps clamp or fail — never silently compound

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-cap-modes-ovl`

A stacked result beyond `maxCumulativeMarkup` clamps to the cap and records the clamp, or fails
closed when the cap is marked hard; a **material multi-link chain without a configured cap is a
publish-time fail** (or a Finance-set default applies). Only the default cap value and the
clamp-vs-hard-fail mode remain open ([`../PRD.md`](../PRD.md) §15).

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-ovl`

All value objects; overlay definitions are frozen catalog content, contract terms frozen
Contracts content.

- **`ScopeClass`** — the ordered class set `customerGroup > partner > orgTier > brand > region > global` (the order *is* the domain fact — O1/O2).
- **`OverlayCandidate`** — a pinned `PriceOverlay` with its scope, class, `precedence`, and `priceOverlayId`.
- **`SurvivorStack`** — the scope-filtered candidates in the total order (§4.2); retains per-candidate filter provenance for diagnostics.
- **`StackOrderKey`** — `(precedence asc, ScopeClass order, priceOverlayId asc)` — the comparator as a value.
- **`ContractOverlay`** — the frozen contract/account override terms (incl. negotiated reserved rates — consumed at step 6 via slice [`05`](./05-commitments-reservations.md), SEAMS M9).
- **`CapPolicy`** — `maxCumulativeMarkup` + mode (`clamp` \| `hard`); absence on a material multi-link chain is a publish-time condition, not a runtime branch.
- **`OverlayLineage`** — per-layer applied id + pre/post amounts, the clamp record if any; feeds `MetadataRecorder` and the snapshot overlay segment.

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-overlays-precedence-ovl`

The steps 4–5 evaluator, registered into the fixed slots (01 §3.2):

- **`ScopeFilter`** — applies the §4.1 mapping table against the frozen context axes; emits the survivor set with per-list match provenance.
- **`OverlayStacker`** — orders survivors by `StackOrderKey` and applies them sequentially over the step-3 model output; flags (in metadata) any within-class equal-precedence overlap that publish validation should have rejected.
- **`ContractOverlayApplier`** — step 5: applies `ContractOverlay` after the stack; `Contract > Partner > Base`; relies on publish-side dimension-integrity and approval bounds (§4.3).
- **`CompositionCapGuard`** — accumulates the chain delta across steps 4–5 and enforces `CapPolicy` (§4.4).
- **`OverlayLineageRecorder`** — writes `OverlayLineage` into evaluation metadata and hands the resolved overlay/`priceOverlay` ids to the `SnapshotComposer` (S1 segment).

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-stack-overlays-ovl`

The **steps 4–5 contract** (internal; pipeline-invoked): `apply_overlays(ModelLineOutcome,
frozen PriceOverlay set, ContractOverlay, ctx) → OverlaidLineOutcome | OverlayProblem`.
Deterministic over the frozen tuple. Problems (this slice's rows in the Design-set taxonomy):
`cap_exceeded_hard` (hard cap breached), `missing_cap_material_chain` (defensive; normally a
publish-time fail), `contract_dimension_violation` (defensive; publish-side guarantee broken)
— all fail-closed; the clamp path is *not* a problem, it is a recorded outcome.

### 3.4 Internal Dependencies

Upstream: [`01-foundation.md`](./01-foundation.md) (pipeline, snapshot composer, lineage
recorder); [`02-selection-eligibility.md`](./02-selection-eligibility.md) (the base row and its
`priceOverlay` axis); [`03-metering-models.md`](./03-metering-models.md) (the model output the
stack applies to). Downstream: [`05-commitments-reservations.md`](./05-commitments-reservations.md)
(step 6 runs on the overlaid amount; negotiated reserved rates arrive via the contract overlay),
[`06-coupons.md`](./06-coupons.md) (coupons apply post-commitment), and the `EmissionGuard`
non-negative check after step 8 — FX + billing-currency coupons (01 §4.4).

### 3.5 External Dependencies

| Dependency | What arrives frozen | Contract |
|------------|--------------------|----------|
| Pricing (Product Catalog) | the pinned `PriceOverlay` set: scope, class, `precedence`, `priceOverlayId`; publish-time precedence validation | [`11-consumer-contracts.md`](./11-consumer-contracts.md); SEAMS O1–O3 |
| Contracts & Agreements | frozen contract/account overlay terms, entitlement/approval bounds, negotiated reserved rates | PRD §6.4 step 5; SEAMS M9 |
| OSS/AMS (tenant identity) | the payer's BSS-resolved `customerGroup` at `t` (authenticated caller claims, frozen into the context) | PRD §6.3 scope mapping; SEAMS O2 |

### 3.6 Interactions and Sequences

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-flow-stack-overlays-ovl`

**Stack overlays** (steps 4–5 of `cpt-cf-bss-rating-seq-evaluate-tariff`):

1. `ScopeFilter`: evaluate each pinned `PriceOverlay` against the §4.1 axis table; collect survivors.
2. `OverlayStacker`: order by `StackOrderKey`; apply **all** survivors sequentially over the step-3 output; record per-layer lineage.
3. `ContractOverlayApplier`: apply the contract/account overlay on the stacked result (`Contract > Partner > Base`).
4. `CompositionCapGuard`: check the cumulative chain delta against `CapPolicy` — clamp-and-record, or fail closed on a hard cap (§4.4).
5. `OverlayLineageRecorder`: lineage → metadata; resolved overlay/`priceOverlay` ids → the S1 snapshot segment; hand to step 6.

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-cap-clamp-ovl`

**Cap clamp**: stacked delta exceeds a `clamp`-mode cap → the amount is clamped **to** the cap,
the clamp is recorded in metadata (pre-clamp amount preserved in lineage), evaluation continues;
under a `hard` cap the same condition fails closed instead ([`../PRD.md`](../PRD.md) §6.4).

### 3.7 Database Schemas and Tables

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-storage-none-ovl`

**None owned.** `PriceOverlay` definitions and precedence live in the pricing gear; contract
overlays live in Contracts; the applied set lives in the sealed snapshot segment and outcome
lineage (01 §3.7).

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-ovl`

Nothing beyond the Foundation posture (01 §3.8); stacking is per-line, in-memory, and
partition-local.

## 4. Additional Context

### 4.1 Scope → Tenant-Axis Mapping (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-scope-mapping-ovl`

Adopted verbatim from [`../PRD.md`](../PRD.md) §17.1 (scope mapping table), including the
`customerGroup` class (SEAMS O2):

| `PriceOverlay.scope` | MUST match (evaluation context) |
|-------------------|--------------------------------|
| `global` | always eligible (subject to plan/SKU applicability) |
| `customerGroup` | payer's BSS-resolved customer group at `t` — resolved from the pricing read model's **membership subject** at the pinned version and frozen into the evaluation context (never live caller claims at evaluation: a period-tick-synthesized unit has no caller — 2026-07-31 review, #40; contract row in slice [`11`](./11-consumer-contracts.md) §3.5, assembly in slice [`14`](./14-unit-synthesis-period-tick.md) §4.3); most-specific class |
| `partner`, `orgTier` | `sellerTenantId` (channel/reseller that sold the subscription) |
| `brand` | Plan/SKU `brandId` at `t` |
| `region` | usage or price-row `region` key |

Axes **not** used as scope filters: `resourceTenantId` (usage tenancy — MUST NOT alone match
partner/orgTier rows); `payerTenantId` / `accountId` (contract/account overlays, step 5);
`sellerTenantId` is consumed only by `scope(partner|orgTier)`.

### 4.2 Stacking and the Total Order (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-stacking-order-ovl`

- Step 4 applies **all** scope-matching survivors as a sequential stack (SEAMS O3, resolved: stack-all; rejected: single-winner).
- Total order: ascending `precedence` (lower first) → for **cross-class** ties (precedence is unique only *within* a class) the class-specificity order `customerGroup > partner > orgTier > brand > region > global` (pricing `design/09`, adopted verbatim — O1) → ascending `priceOverlayId` as the final **within-class** stable tie-break.
- Equal `precedence` among lists with overlapping scope within one class MUST be rejected at publish (fail-closed); the runtime order is the safety net that still produces a single deterministic result.
- **Single-layer application semantics**: each survivor contributes **exactly one adjustment line** to the stack layer — the overlay is a **line container** (pricing D-42, CONFIRMED 2026-07-28), and the layer's adjustment is the **most-specific line** of that overlay for the priced row: `(planId, targetSku)` > `(planId)` > list-default (resolution rule published as pricing `inst-plv-lines`; an overlay whose lines all miss the priced row contributes nothing). **Cohort eligibility filter (pricing D-78, adopted 2026-08-01 — T-D-30)**: a line MAY carry an optional `cohort` filter on its key — **unset ⇒ `existing_grandfathered` rows are exempt from the line** (the fail-safe default: a partner markup never silently reprices a cohort the grandfathering machinery exists to protect), a set value ⇒ the line reaches **only** that generation's rows; the filter evaluates against the priced row's `cohort` axis before line selection. The selected line applies to the running post-model line amount — sequential compounding, layer *n+1* sees layer *n*'s output. A percentage line (basis points) scales the amount; a per-currency **absolute amount line** (pricing D-08, `pricing_price_overlay_line_amount`) applies its published value for the line's price currency. Adjustments are **charge-line-level** — the published overlay model carries no per-band adjustment, so banded lines are adjusted on the post-band total. Publish-side guarantee relied on: every absolute line covers the priced row's currency **at the time the line is published** (`ADJUSTMENT_CURRENCY_NOT_COVERED` rejected at pricing publish). An uncovered currency **is** reachable at runtime and is **not** an error: publishing a base row in a **new** currency later flags the targeting amount-based lines `coverage_incomplete` (+ a Warn alarm) rather than rejecting, and pricing's normative remediation semantics are to **drop that overlay from the stack for that currency and continue** — the market resolves from the remaining overlays / base until the operator adds the per-currency value (pricing D-08 drift path, `inst-plv-adjustment`; pricing design/09 §DoD). Rating MUST follow that rule, **not** fail the line closed: the earlier defensive fail-closed reading would have made every subscription in a newly launched currency unrateable behind a Warn-level alarm (pricing D-66, 2026-07-29 cross-gear review fix).
- **Recompute rule (T-D-13)**: when step 6's reservation split re-bands the on-demand remainder ([`05-commitments-reservations.md`](./05-commitments-reservations.md) §4.2), steps 4–5 **re-apply as part of the steps-3–5 remainder re-run**: the same frozen survivor set, total order, and adjustment values compound over the re-banded remainder amount — only the base amount changes. The pre-split full-`Q` pass is superseded (both passes recorded in lineage); the reserved-rate portion is **not** re-overlaid — its rate is already the post-step-5 effective value (05 §4.3), **and the step-4 stack does not reach it either (intended, T-D-23)**: a channel/partner adjustment applies to the on-demand remainder only — the reserved rate is a committed term (catalog- or Contracts-sourced per M9), not a base amount for channel economics; the `CompositionCapGuard` evaluates on the authoritative re-run amounts.
- ~~Open (O3-confirm)~~ — **closed 2026-07-28** (this slice had kept the open after the registers closed it — 2026-07-31 review, #26): pricing confirmed "most-specific-wins" is class selection + tie-break, never overlay exclusivity — `inst-plv-class-tiebreak` states stack-all normatively; [`../SEAMS.md`](../SEAMS.md) O3 "fully closed", [`../DECISIONS.md`](../DECISIONS.md) T-D-02.

### 4.3 Contract Overlay Precedence (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-contract-overlay-ovl`

- Step 5 applies contract/account-level overrides **after** the step-4 stack; contract terms outrank partner lists: `Contract > Partner price overlays > Catalog base`.
- Overrides MUST NOT introduce metering dimensions absent from the published Plan/SKU revision — contract publish validation rejects fail-closed; the evaluator relies on the guarantee (defensive `contract_dimension_violation` problem if it is ever observed broken).
- Customer-layer changes MUST NOT silently weaken audit controls (Contract workflow + optional Finance approval — upstream obligations; lineage records what applied).
- Negotiated reserved-instance rates ride this overlay (Contracts SoR) and are consumed by step 6 — the reserved-rate two-source split is slice [`05`](./05-commitments-reservations.md)'s M9 rule.

### 4.4 Bounded Composition — Anti-Drift Cap (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-anti-drift-cap-ovl`

- The cumulative markup/discount across the full partner → reseller → customer chain is bounded by `maxCumulativeMarkup`.
- Exceeding a `clamp` cap ⇒ clamp to the cap + record the clamp in metadata (pre-clamp amount in lineage); exceeding a `hard` cap ⇒ fail closed. Silent unbounded compounding is forbidden in every mode.
- A **material multi-link chain** with no configured cap MUST fail closed **at publish** (or a Finance-set default applies); a publish-time warning is acceptable only for a single-link, non-material overlay.
- **Open (PRD §15):** the default cap value and the clamp-vs-hard-fail mode are a Program/Finance workshop decision; the step-4 normative behavior above does not depend on it.

## 5. Traceability

**Traces to**: `cpt-cf-bss-rating-fr-priceoverlay-scope-mapping`, `cpt-cf-bss-rating-fr-overlay-stacking`, `cpt-cf-bss-rating-fr-customer-contract-overlay`,
`cpt-cf-bss-rating-fr-bounded-composition-cap`

- **PRD**: §6.3 `fr-priceoverlay-scope-mapping`; §6.4 `fr-overlay-stacking`, `fr-customer-contract-overlay`, `fr-bounded-composition-cap`; §17.1 steps 4–5 + scope-mapping table.
- **Seams**: O1 (class order as cross-class tie-break), O2 (`customerGroup` class), O3 (**fully closed 2026-07-28** — stack-all confirmed both sides) — [`../SEAMS.md`](../SEAMS.md); S1 (overlay segment), M9 (negotiated rate pass-through), W2 (replay from the pin).
- **Decisions**: T-D-02 — [`../DECISIONS.md`](../DECISIONS.md).
- **ADR**: [`../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md`](../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md) (base-row `priceOverlay` axis vs overlay stack split).
- **Related slices**: [`01-foundation.md`](./01-foundation.md) (pipeline, snapshot segment, lineage, non-negative guard), [`02-selection-eligibility.md`](./02-selection-eligibility.md) (base row in), [`03-metering-models.md`](./03-metering-models.md) (model output in), [`05-commitments-reservations.md`](./05-commitments-reservations.md) (step 6 on the overlaid amount; M9), [`06-coupons.md`](./06-coupons.md) (step 7 after), [`10-governance-asc606.md`](./10-governance-asc606.md) (publish-time validator registration, G1).
