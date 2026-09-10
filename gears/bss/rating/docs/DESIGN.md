Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Commercial Pricing Logic — Technical Design (canonical index) -->
<!-- Related: ./PRD.md, ./SEAMS.md, ./ADR/, ./design/ | Owners: BSS Rating team -->

# Technical Design — Rating (Evaluation Core + Pipeline)

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

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-design-main`

> **Canonical design entry point and index.** This document is the rating gear's top-level
> technical design and the anchor for spec traceability. The design is a **set of slice documents**
> under [`design/`](./design/): the **evaluation core** (slices 01–11 — a shared Evaluation
> Foundation plus per-step evaluator slices along the §17.1 rule order) and the **operational
> pipeline** (slices 12–16, per ADR-0002). This page is the single index over that set and
> delegates slice-level detail to the slice documents.
>
> **Terminology bridge (ADR-0002)**: inside the evaluation-core slices, historical "**Tariffs**"
> reads as *the evaluation core* (`rating-core`), and historical neighbour-"**Rating**" (the
> then-separate rating-engine) reads as *the rating pipeline* — one gear since consolidation;
> (2026-07-28 fix: the mechanical rename had collapsed both historical terms to "Rating",
> leaving the bridge mapping a word to itself); full prose re-voicing is a tracked
> migration residue ([`DECISIONS.md`](./DECISIONS.md)).
>
> **Status**: evaluation core (01–11) and operational pipeline (12–16) authored, pre-Design-lock.

## 1. Architecture Overview

### 1.1 Architectural Vision

The **rating gear** is the BSS usage-rating domain (ADR-0002): **rating-core** — the
commercial-price evaluation engine — plus the operational **rating pipeline**. Given a frozen
evaluation context at `t`, the core resolves effective rates, the pricing model, overlay winners,
commitment/reservation effects, coupons, and FX to a **resolved price outcome** plus a
`pricingSnapshotRef` — **byte-for-byte reproducibly**; the pipeline supplies the frozen inputs and
persists the outcome (slices 12–16). The gear is **not** an authoring System of Record: the
pricing gear owns the catalog (the canonical scope key, `PriceWindow`, `PriceOverlay`,
`CatalogVersion`, and publish governance); the core **adopts** those and evaluates over a frozen
snapshot ([`PRD.md`](./PRD.md) §1.1).

The complementarity contract with the pricing gear — the 8-axis key, cohort selection, overlay
stacking, snapshot composition, single governance engine, launch-scope models — is frozen in
[`SEAMS.md`](./SEAMS.md); every slice here implements the rating side of a resolved seam.

### 1.2 Architecture Drivers

- **Determinism**: a pure-function core over frozen inputs `(window-aggregated Q, pricingSnapshotRef, fxTableVersion)` — identical across replay, recompute, and cross-region batch workers.
- **Adopt, don't fork**: the canonical scope key, `PriceWindow` machinery, and publish governance are the pricing gear's SoR; Rating consumes them verbatim (no divergent key, no second approval engine) — see ADR `cpt-cf-bss-rating-adr-scope-key-adoption`.
- **One rating gear (accepted 2026-07-11)**: this design set is the **evaluation core** of the consolidated `rating` gear — the core is a no-I/O `rating-core` crate, the operational pipeline (mediation, `Q`, dedup, period tick) joined as pipeline slices 12–16 (authored 2026-07-15), and "tariff" returns to the Pricing vocabulary — see ADR `cpt-cf-bss-rating-adr-rating-gear-consolidation`. The gear-local migration is done; the one open residue is the **pricing-side prose rename** (ADR-0002 Commit C — `tariffs` still appears in the living pricing docs) tracked in [`DECISIONS.md`](./DECISIONS.md).
- **Snapshot-only hot path**: no mutable catalog re-query at evaluation/correction time; open-period re-resolution replays the pinned snapshot.
- **Scale**: horizontal per-partition evaluation, no cross-partition locks on the hot path.

### 1.3 Architecture Layers

A shared **Evaluation Foundation** (`01-foundation`) carries the pure-function core, the adopted
scope key, determinism, and `pricingSnapshotRef` composition. Each subsequent slice is an
**evaluator** contributing one region of the §17.1 rule order, running through the Foundation over
a frozen snapshot. The numeric prefix follows the §17.1 evaluation order, then the period-level and
cross-cutting slices.

| Slice | Title | PRD §6 | Seams owned |
|-------|-------|--------|-------------|
| [`design/01-foundation.md`](./design/01-foundation.md) | Evaluation Foundation | §6.1, §17.1 core | K1, K3, S1, M7, W2, M11 |
| [`design/02-selection-eligibility.md`](./design/02-selection-eligibility.md) | Base Selection & Eligibility | §6.3, §6.5 | K1, K2, K4, K5, F1 |
| [`design/03-metering-models.md`](./design/03-metering-models.md) | Metering & Pricing Models | §6.2, §6.5, §6.7 | M1, M2, M3, M5, M6, M7, M10, M11 |
| [`design/04-overlays-precedence.md`](./design/04-overlays-precedence.md) | Overlays & Precedence | §6.4 | O1, O2, O3 |
| [`design/05-commitments-reservations.md`](./design/05-commitments-reservations.md) | Commitments & Reservations | §6.6 | M8, M9 |
| [`design/06-coupons.md`](./design/06-coupons.md) | Coupons | §6.8 | — |
| [`design/07-currency-fx.md`](./design/07-currency-fx.md) | Multi-Currency & FX | §6.9 | S1 |
| [`design/08-retroactivity-corrections.md`](./design/08-retroactivity-corrections.md) | Retroactivity & Corrections | §6.10 | W2 |
| [`design/09-period-plan-change.md`](./design/09-period-plan-change.md) | Period & Plan-Change Obligations | §6.11 | P1, P2 |
| [`design/10-governance-asc606.md`](./design/10-governance-asc606.md) | Governance & ASC 606 | §6.12 | G1, B1 |
| [`design/11-consumer-contracts.md`](./design/11-consumer-contracts.md) | Consumer & Integration Contracts | §9 | C1, S1, B1 |
| [`design/12-usage-ingestion-normalization.md`](./design/12-usage-ingestion-normalization.md) | Usage Ingestion & Normalization *(pipeline)* | §9.2 | — |
| [`design/13-q-store-attribution.md`](./design/13-q-store-attribution.md) | Windowed Q Store & Attribution *(pipeline)* | §9.2 | M7 (writer side) |
| [`design/14-unit-synthesis-period-tick.md`](./design/14-unit-synthesis-period-tick.md) | Unit Synthesis & Period Tick *(pipeline)* | §9.2 | — (T-D-15) |
| [`design/15-rated-output-balance-effects.md`](./design/15-rated-output-balance-effects.md) | Rated Output, Delta Dedup & Balance Effects *(pipeline)* | §9.2 | — (T-D-10/11) |
| [`design/16-billing-handoff-operations.md`](./design/16-billing-handoff-operations.md) | Billing Handoff & Operations *(pipeline)* | §9.2 | — |

## 2. Principles & Constraints

### 2.1 Design Principles

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-pure-function-core`
  No I/O and no mutable-state read on the evaluation path; `Q` aggregation is Rating's (single-writer per `(subscription, meter, dimensionKey, window)`), consumed frozen.
- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-fixed-rule-order`
  §17.1 steps 1-9 are invariant; there is no reordering knob.
- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-fail-closed`
  No silent fallback on an unmatched window, ambiguous mapping, missing policy, or missing `periodState`.
- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-adopt-the-sor`
  The canonical scope key, `PriceWindow`, `PriceOverlay`, and publish governance are consumed verbatim from the pricing gear.

### 2.2 Constraints

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-posted-immutability`
  Corrections flow as deltas; they never mutate posted invoices or prior outputs.
- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-domain-boundaries`
  No tax, no revenue recognition, no coupon lifecycle, no spend enforcement (PRD §5.2).
- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-stateless-hot-path`
  The evaluation core (`rating-core`) owns no authoritative persistent store and holds no mutable state on the hot path; all commercial inputs arrive frozen (§3.7). The **pipeline** side of the gear does own authoritative stores (usage dedup — slice 12; windowed `Q` — slice 13; rated-output + delta-dedup index — slice 15); this constraint scopes the core, not the gear.

## 3. Technical Architecture

### 3.1 Domain Model

_TBD (skeleton)._ The evaluation-context aggregate, the resolved-price-outcome value object, and
the `pricingSnapshotRef` composite are owned by `01-foundation`; per-step domain shapes by their
slices. Rating holds no catalog entities — those are frozen inputs from the pricing gear.

### 3.2 Component Model

The evaluation core is the eleven slices in §1.3, each a pure-function contributor to one region
of the §17.1 order, composed by the Foundation's evaluation pipeline; the five pipeline slices
(12–16, ADR-0002) carry the operational components. Each carries a stable
`cpt-cf-bss-rating-component-{slug}` ID; internals live in the slice documents.

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-foundation`
  Evaluation Foundation: pure-function core, adopted scope key, determinism, `pricingSnapshotRef` composition.
- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-selection-eligibility`
  Steps 1-2 selection, eligibility class order, cohort generation selection.
- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-metering-models`
  Step 3 mapping + model formulas + tier window + dimensional/composite.
- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-overlays-precedence`
  Steps 4-5 PriceOverlay stacking + contract overlay + anti-drift cap.
- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-commitments-reservations`
  Step 6 drawdown + reservation flavors.
- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-coupons`
  Step 7 coupon application + stacking.
- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-currency-fx`
  Step 8 FX policy.
- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-retroactivity-corrections`
  Posted-period protection + late-arrival replay + reversals.
- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-period-plan-change`
  Floor/cap + proration obligations.
- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-governance-asc606`
  Validator registration + ASC 606 + rev-share pass-through.
- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-consumer-contracts`
  Rating / pricing / Subscriptions / Finance / Promotions / Billing contracts.
- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-usage-ingestion`
  Pipeline: usage intake, normalization, session merge, authoritative usage dedup store.
- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-q-store`
  Pipeline: windowed `Q` single-writer store, per-slice attribution + `bandOffsetQ`, re-materialization.
- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-unit-synthesis`
  Pipeline: evaluation-unit synthesis, period tick, frozen-context assembly, bounded cascade routing.
- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-rated-output`
  Pipeline: rated-output persistence, RatedCharge/BillableItem mapping, delta dedup, `CommitmentBalanceEffect` publication.
- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-billing-handoff`
  Pipeline: Billing handoff (outbox), `periodState` relay, lanes/backpressure/replay, NFR verification.

### 3.3 API Contracts

_TBD (skeleton)._ The conceptual evaluation contract (context → resolved outcome + `pricingSnapshotRef`)
and the integration contracts (Rating handoff; Pricing read-model input; Subscriptions, Finance-FX,
Promotions, Billing, Contracts & Agreements) are owned by `11-consumer-contracts` and PRD §9.
No simulation/what-if surface exists in this skeleton: `usecase-finance-simulation` (candidate
future PriceWindows against a sample profile) is **deferred to Follow-on** (T-D-32, 2026-08-01 —
no slice claims it, and evaluating a *draft/candidate* window conflicts with the pin discipline;
the partner-overlay use case's "simulate against sample usage" step rides the same gate).

### 3.4 Internal Dependencies

Every core slice depends on `01-foundation`. Evaluation order (data flow): `02 → 03 → 04 → 05 → 06 → 07`
per §17.1 steps 1-8; `08` (retroactivity) and `09` (period/plan-change) wrap the per-line flow;
`10` (governance) gates publish; `11` (contracts) is the boundary surface. Pipeline slices
(ADR-0002) wrap the core operationally: `12` feeds normalized usage, `13` materializes `Q`,
`14` synthesizes evaluation units and assembles the frozen context, `15` persists outcomes and
publishes balance effects, `16` hands off to Billing.

### 3.5 External Dependencies

- **Pricing (Product Catalog) gear** — SoR for the scope key, `PriceWindow`, `PriceOverlay`, `CatalogVersion`; produces the `PriceWindow*` events and the frozen read model. `CatalogVersionPublished` comes from the **registry (products gear)**, the sole `CatalogVersion` incrementer (corrected 2026-07-29 — pricing D-66).
- **Subscriptions** — phase, eligibility inputs, seat count, plan-change policy.
- **Finance** — FX tables and lock policies. **Promotions** — frozen coupon snapshots. **Billing** — `periodState`, floor/cap and rounding execution.

### 3.6 Interactions & Sequences

The normative step-1-9 evaluation sequence lives in PRD §17.1; per-slice sequences are TBD in the
slice documents.

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-seq-evaluate-tariff`
  Deterministic step-1-9 evaluation of one line from a frozen context to a resolved outcome + `pricingSnapshotRef`.
- [ ] `p2` - **ID**: `cpt-cf-bss-rating-seq-open-period-reresolve`
  Late-arriving usage re-resolution replayed strictly from the pinned snapshot, emitting delta adjustments.

### 3.7 Database schemas & tables

**Core: none owned; pipeline: three stores.** The evaluation core (`rating-core`, slices 01–11) is
a pure-function evaluator over frozen inputs and holds **no authoritative persistent store**; any
evaluation-side cache (e.g. a resolved-window cache invalidated by `PriceWindow*` events) is
non-authoritative and defined per slice. Post-consolidation (ADR-0002 / T-D-16) the **pipeline** half
of the gear owns the authoritative stores: the **usage dedup store** (slice 12), the **windowed `Q`
store** (slice 13, single-writer per `(subscription, meter, dimensionKey, window)`), and the
**rated-output store + delta-dedup index** (slice 15). External commercial state stays with its SoR:
the pricing gear (catalog), Contracts (`commitmentPools[]` balances), and Finance (FX).

### 3.8 Deployment Topology

One deployable: **`rating-core`** is a pure, I/O-free crate inside the `rating` gear (ADR-0002 —
supersedes the earlier placement note that treated the evaluation core as a logical module of Rating; the one-deployable,
no-separate-evaluation-service constraint is preserved and strengthened to a crate boundary).
Horizontal per-partition evaluation with no cross-partition locks on the hot path (PRD §7.1, §14).

## 4. Additional context

**Cross-cutting normatives** (frozen resolutions from [`SEAMS.md`](./SEAMS.md), binding every slice):

- **Canonical scope key (K1-K5):** selection and non-overlap use the pricing 8-axis key `(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)`; `phase` is a `phase_id`; grandfathering selects the generation by the pinned price id's `cohort`.
- **Overlays (O1-O3):** step 4 **stacks** all PriceOverlay survivors; the class-specificity order `customerGroup > partner > orgTier > brand > region > global` breaks ties, not exclusivity.
- **Snapshot (S1):** one `pricingSnapshotRef`, **four writers** (D-66 producer split; review #10) — the registry commits `catalogVersion`, pricing pre-stamps its catalog subset, Rating (composition SoR) adds overlay/coupon/FX-lock/`commitmentReservation` (T-D-09), Subscriptions freezes the `(currency, region)` binding.
- **Determinism / corrections (W2, M7):** replay strictly from the pinned snapshot; counter key `(subscription, meter, dimensionKey, window)`.
- **Governance (G1):** a single pricing Slice 5 approval engine; Rating registers fail-closed validators, never a second workflow; ledger `dual_control` stays a separate bounded context.
- **Models (M1-M5):** `{flat, per_unit, graduated, volume, package}`; per_unit, package, and composite are in launch; Volume Variant B not authorable; hybrid/committed are compositions.
- **Enums (P1-P2):** `prorationBasis` (incl. `none`) and `billingAnchorPolicy` adopted verbatim (CI gate `pricing.contracts.enum_drift`).

**Later gear decisions binding every slice** (2026-07-11 / 2026-07-16 / 2026-07-28 / 2026-08-01 — the list above is the frozen 2026-07-10 seam set; full text in [`DECISIONS.md`](./DECISIONS.md)):

- **The 2026-07-11 Design-pass wave (T-D-09…T-D-16)** — omitted from this page until the 2026-07-31 review (#25), though several rows amend the seam set above: the **eighth snapshot segment `commitmentReservation`** (T-D-09 — amends the S1 bullet); the **balance write-back protocol** — `CommitmentBalanceEffect`s, Contracts-serialized `balanceVersion`, delta-only cascades (T-D-10); **delta-dedup owner = Rating** (T-D-11); **sub-window slices + frozen `bandOffsetQ`** with the correction key gaining the slice coordinate (T-D-12); the **steps-3–5 reservation-remainder recompute** (T-D-13); **frozen `poolType ∈ {prepaid_drawdown, committed_rate}` + true-up formulas** (T-D-14); **period-driven units synthesized by Rating's period tick** (T-D-15); and the **gear consolidation** itself (T-D-16 / ADR-0002).
- **Level-based aggregation (T-D-17, joint with pricing D-44) — a `p1` launch capability:** `aggregationFunction ∈ {sum, peak, time_weighted}` with `aggregationGranularity ∈ {hour, day}`; for non-`sum` the window `Q` is the **sum of granule folds**, so it stays additive and every counter invariant (M7 key, supersession continuity, `bandOffsetQ`, band/package math, delta-only corrections) is untouched. Supersedes the former "sum-only at launch" posture. No composite co-occurrence at launch.
- **One-time charges are not rated (T-D-18):** no evaluation unit is synthesized for `one_time` / `one_time_setup`; the three unit kinds remain exhaustive. Subscriptions/Billing bill them at the qualifying instant from the frozen snapshot; one-time rows still resolve in step-2 selection for coverage/preview/quote.
- **Overage-rate selector (T-D-19):** a frozen `overageRate` on the `commitmentReservation` segment selects flat-rate vs banded pricing of the post-pool residual — the field's **presence** is the selector.
- **Coupon cross-scope exclusivity (T-D-20) and its launch gate (T-D-22):** with a `line_total` candidate present, `exclusive_best` widens to one coupon per plan per period. **At launch `line_total` fails closed** — step 7 is per-line and every evaluation unit is sub-plan, so no plan-scoped base exists; T-D-20 is inert until that base is pinned with Promotions.
- **Administrative re-rate (T-D-21):** the one sanctioned exception to strict pin replay — a *policy* correction replays over the **superseding** snapshot; input corrections stay strictly on the pin — **which it advances (T-D-24, 2026-08-01)**: after a re-rate, later input corrections replay the superseding pin, so a delta never embeds the negation of an approved repricing.
- **The 2026-08-01 billing-domain wave (T-D-23…T-D-32; review doc `reviews/2026-07-31-billing-domain-review.md`)**: the reservation-remainder **band axis** (`remainderOffsetQ`, window-cumulative — T-D-23); the pin-of-record advance (T-D-24); **capacity-charge accrual by covered granules** (T-D-25); the **boundary-attribution pair** — the `calendar_days_actual` boundary day and a straddling granule both belong to the slice that opened them, fractions sum to 1 (T-D-26); **pool-observation effects + Contracts-side over-draw detection** (T-D-27, joint); the **periodState sequence fence** (T-D-28, joint with Billing); the adoptions of pricing **D-113** (`usageCounterOnPlanChange`, T-D-29), **D-78** (overlay cohort filter, T-D-30) and **D-114** (prefix-closed pin frontier, T-D-31); and the **finance-simulation deferral** (T-D-32). **Veto round 2026-08-01: T-D-18…T-D-28 and T-D-32 all confirmed per-item by the product owner** (T-D-27/T-D-28 as joint — the Contracts/Billing cross-PRD mirrors stay owed).

**ADR index:**

- [`ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md`](./ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md) (`cpt-cf-bss-rating-adr-scope-key-adoption`) — adopt the pricing 8-axis canonical scope key + cohort selection rather than define a Rating key.
- [`ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md`](./ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md) (`cpt-cf-bss-rating-adr-rating-gear-consolidation`) — consolidate Rating (evaluation core) and the Rating pipeline into one `rating` gear; core = no-I/O crate; naming + migration map (T-D-16).
- _Further ADRs (determinism/replay model; snapshot composition split; single governance engine) to be seeded during Design._

## 5. Traceability

- **Requirements:** [`PRD.md`](./PRD.md) (§6 functional, §17.1 normative rule order).
- **Cross-gear contract:** [`SEAMS.md`](./SEAMS.md) (seam register + decision log).
- **Decisions:** [`DECISIONS.md`](./DECISIONS.md) (Rating decision register).
- **Slices:** [`design/`](./design/) (per-slice designs) and [`design/README.md`](./design/README.md).
