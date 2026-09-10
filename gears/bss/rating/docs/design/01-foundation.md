Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Evaluation Foundation (pure-function core) (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Pricing (Product Catalog), Rating, Subscriptions, Finance, Promotions, Billing | Downstream: Rating | Owners: BSS Rating team -->

# DESIGN — Evaluation Foundation (pure-function core) (Slice 1)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-design-foundation`

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
  - [4.1 Adopted Canonical Scope Key (normative)](#41-adopted-canonical-scope-key-normative)
  - [4.2 Determinism and Idempotency Contract (normative)](#42-determinism-and-idempotency-contract-normative)
  - [4.3 pricingSnapshotRef Composition (normative)](#43-pricingsnapshotref-composition-normative)
  - [4.4 Emission Guards (normative)](#44-emission-guards-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

The Evaluation Foundation is the pure-function core every Rating capability runs *through* —
the mirror image of the pricing gear's *publish through the engine*: here the contract is
**evaluate through the core**. It owns the **evaluation pipeline** (the invariant §17.1 step
order with registered step evaluators), the **evaluation context** and **evaluation unit**
shapes, the **adopted** pricing 8-axis canonical scope key, the **determinism contract**
(byte-identical replay over frozen inputs), `pricingSnapshotRef` **composition** (Rating is
the composition SoR), the usage/delta **idempotency** keys, and the **emission guards**
(non-negative line, full-precision emission, no rounding). It owns **no step policy**: what a
tier band means, how overlays stack, when a coupon applies — all live in the step slices
(02–07), each of which registers its evaluator into the pipeline under the invariants defined
here.

Everything commercial arrives **frozen**: the catalog read model pinned by `pricingSnapshotRef`
(pricing gear), the window-aggregated quantity `Q` (Rating, single-writer), the FX table
version (Finance), coupon snapshots (Promotions), and `periodState` (Billing). The Foundation
never queries mutable catalog state on the hot path; open-period corrections **replay the
pinned snapshot** (SEAMS W2). There is no authoritative store in rating-core — the resolved outcome
plus its snapshot ref *is* the output, and Rating owns its persistence
([`../PRD.md`](../PRD.md) §1.1, §4.1, §6.1).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-fr-deterministic-evaluation-api` | One conceptual `evaluate(ctx, t)` contract: frozen context in → **resolved price outcome** (rates, model kind, tier thresholds, overlay winners) + `pricingSnapshotRef` + evaluation metadata out; replay-safe by construction (§4.2). |
| `cpt-cf-bss-rating-fr-evaluation-order` | The pipeline hard-codes the §17.1 step order (1 composition/phase → 2 base row → 3 meter/granularity → 4 overlay stack → 5 contract → 6 commitment/reservation → 7 coupon → 8 FX → 9 emit); step evaluators register into fixed slots; **there is no reordering configuration surface at all**. |
| `cpt-cf-bss-rating-fr-single-outcome-determinism` | Determinism is stated over the **evaluation unit** (§4.2): per-event record or window-aggregated `Q` per `(subscription, meter, dimensionKey, window)`; given `(window-aggregated inputs, pricingSnapshotRef, fxTableVersion)` the monetary outcome is byte-identical across replay/recompute/regions; concurrent re-resolve serializes on the partition key. |
| `cpt-cf-bss-rating-fr-snapshot-carry` | The `SnapshotComposer` assembles the canonical field list (§4.3): pricing pre-stamps the catalog subset, Subscriptions freezes the `(currency, region)` binding, the pipeline appends the eval-time segments (overlay ids, coupon ids + stacking, FX lock, commitment/reservation set); every emission carries stable `{skuId, planId, priceId}`. |
| `cpt-cf-bss-rating-fr-idempotency` | Two key families (§4.2): the usage idempotency key (Rating dedup authoritative) and the **correction key** `(unitKey[, slice], prior-rated-version, snapshot)` for deltas (`unitKey` = usage counter key or period-driven key, §4.2) — a re-rate retry replays, never double-adjusts; delta dedup enforced by the pipeline (§2.2). |
| `cpt-cf-bss-rating-fr-non-negative-price` | The `EmissionGuard` clamps/credits a would-be-negative resolved line **after step 8 (FX + the billing-currency coupon pass)** and **before** period-level floor/cap (§4.4) — a billing-currency `fixed_amount` coupon can drive a line negative at step 8, so the guard clamps the post-FX amount, never the pre-FX one (slices 06 §4.2 / 07 §4.4); clamp-vs-credit policy is a §15 open. |
| `cpt-cf-bss-rating-fr-separation` | The core is side-effect-free: it never mutates Usage or posted invoices; retro outcomes leave as deltas via the Adjustment path; reversal math (pool refill, `Q` decrement) is delegated to slices 05/08 under the §4.2 keys. |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-throughput-latency` (p95 ≤ 100ms lookup, < 1s path, ≥ 10M ev/day/region) | Pipeline + pinned-snapshot resolution | Pure-function hot path over a pre-resolved snapshot; no I/O inside steps; lookup cost is the read-model pin, not evaluation | Load test; **targets provisional — NFR workshop** ([`../PRD.md`](../PRD.md) §7.1) |
| `cpt-cf-bss-rating-nfr-horizontal-scale` | Partitioning contract | Per-partition ordering on `(subscription, meter, dimensionKey, window)`; zero cross-partition locks; re-resolve serializes per key | Design + load test |
| `cpt-cf-bss-rating-nfr-resilience` | Idempotency keys + fail-closed | Retries replay the same keys to the same outcome; missing input (`periodState`, coupon policy, FX record) fails closed, never guesses | Chaos/retry test |
| Decimal precision of emitted amounts | `EmissionGuard` | Amounts emitted at full intermediate precision; **the concrete DECIMAL precision is fixed in this Design before lock** ([`../PRD.md`](../PRD.md) §4.1) | **Open — set with Billing** |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-bss-rating-adr-scope-key-adoption` | Adopt the pricing 8-axis canonical scope key verbatim (selection + non-overlap); cohort generation selected by the pinned price id; no Rating-local key (SEAMS K1–K5). |
| `cpt-cf-bss-pricing-adr-canonical-scope-key` (adopted) | The key definition itself — the manifest key extended additively; the pricing gear is its SoR. |
| `cpt-cf-bss-pricing-adr-grandfathering-cohort-axis` (adopted) | `cohort` = the cutover instant; Rating resolves the generation by the cohort of the subscription's pinned price id. |
| `cpt-cf-bss-pricing-adr-pricewindow-consolidation` (adopted) | `PriceWindow*` events are produced by the pricing gear; Rating consumes all four (incl. `Cancelled`) as read-only resolution inputs. |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-fnd`

```text
Step evaluators (slices 02–07)    selection-eligibility · metering-models · overlays ·
        │  (register into fixed §17.1 slots)          commitments · coupons · fx
        ▼
Evaluation pipeline (Foundation)  EvaluationPipeline · ScopeKeyAdapter · SnapshotComposer ·
                                  DeterminismGuard · EmissionGuard · MetadataRecorder
        │
        ▼
Frozen inputs (external SoRs)     pinned catalog read model (pricing) · windowed Q (Rating) ·
                                  FX tables (Finance) · coupon snapshots (Promotions) ·
                                  periodState (Billing) · phase/eligibility ctx (Subscriptions)
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | Step evaluators contributed by slices 02–07; period wrappers 08–09 | Rust modules in the `rating` gear (rating-core crate) |
| Domain | The pipeline, context/unit/outcome shapes, adopted key, determinism + snapshot contracts | Rust; GTS + Rust domain structs |
| Infrastructure | **None authoritative** — a non-authoritative resolved-window cache invalidated by `PriceWindow*` events; metadata/lineage emitted with the outcome | In-process cache; Rating persistence |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Pure function, frozen inputs

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-pure-function-fnd`

No I/O and no mutable-state read inside the pipeline; every commercial input is frozen before
entry (snapshot pin, windowed `Q`, `fxTableVersion`, coupon snapshot, `periodState`). Absence
of a required input fails closed — never a guess, never a default.

#### One order, one outcome

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-one-order-fnd`

The §17.1 step order is compiled in, not configured. Given one evaluation unit and one frozen
input tuple there is exactly one byte-identical outcome (§4.2), on every worker, in every
region, at any later replay time.

#### Adopt the SoR, compose the snapshot

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-adopt-compose-fnd`

The scope key, windows, price overlays, and publish governance are the pricing gear's; Rating
adopts them verbatim (§4.1) and owns exactly one artifact of record: the **composed**
`pricingSnapshotRef` (§4.3).

### 2.2 Constraints

#### No authoritative store

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-no-store-fnd`

rating-core persists nothing authoritative (§3.7). The resolved outcome is handed to Rating;
commitment balances live in Contracts; `Q` lives in Rating; catalog state in the pricing gear.

#### UTC and ISO 4217 minor-unit inputs

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-utc-money-fnd`

All effective dating, window boundaries, and anchor math are UTC. Price amounts arrive as ISO
4217 minor units from the snapshot; the pipeline computes at full intermediate precision and
never rounds (Billing rounds — §4.4).

#### Delta dedup owner: Rating

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-delta-dedup-owner-fnd`

The correction key (§4.2) makes deltas idempotent; the **owner of delta dedup is Rating**
(**decided 2026-07-11, T-D-11** — it already owns usage dedup, the `Q` store, and rated-output
persistence). Enforcement: Rating persists delta outcomes keyed by the correction key; a retried
delta with a known key returns the recorded outcome and is never re-emitted downstream; Billing
additionally treats Adjustment consumption as idempotent on the same key (defense in depth).
This satisfies the PRD §6.1 requirement that the owner be named before the Adjustment path ships.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-fnd`

- **`EvaluationContext`** — the frozen input aggregate: tenant axes (`resourceTenantId`, `payerTenantId`, `sellerTenantId`), subscription/plan linkage + active `phase_id`, SKU/meter + `dimensionKey`, quantity or time slice (post-granularity), `tierAggregationWindow`, `t` (UTC), currency/region/brand scope, `periodState`, optional `reservationMatch`, optional `(changeEffectiveAt, changeMode)`, and the snapshot identifiers.
- **`EvaluationUnit`** — what determinism quantifies over, three kinds (§4.2): a single normalized `UsageRecord` (`per_event`); the window-aggregated `Q` for `(subscription, meter, dimensionKey, window)` — per sub-window slice when the window is split; a **period-driven unit** (recurring lines, capacity-flavor charges, true-up surfacing) keyed `(subscription, priceId, chargeKind, lineKey, AnchorPeriod)`. The three kinds are exhaustive: `one_time` / `one_time_setup` charges have **no** evaluation unit — billed at sale by Subscriptions/Billing (T-D-18).
- **`ResolvedPriceOutcome`** — effective rates, model kind, tier thresholds, overlay winners + stack lineage, commitment/reservation effects, applied coupon ids + pre/post amounts, FX policy record, `performanceObligationRef`/`sspSnapshotPointer` (nullable), and the composed `pricingSnapshotRef`.
- **`pricingSnapshotRef`** — the four-writer composite (§4.3 — registry, pricing, Subscriptions, Rating); immutable once emitted.
- **`lineKey`** — the **billable component-interval coordinate** within a subscription, **value rule adopted verbatim from subscriptions SUB-D-21 (T-D-34, 2026-08-01)**: `plan#n` for the n-th `PlanLink` interval, `addon:{addOnId}#n` for the n-th interval of that add-on — `n` = the component's 1-based lifetime interval ordinal, assigned at interval open, immutable. Stability: a multi-period interval keeps its key; an in-place `changePlan` opens `plan#n+1` (a mid-period change ⇒ two period-driven units that period); a re-added add-on gets a fresh ordinal; `updateQuantity` opens no interval; **stable across sub-window slices** (a split changes `priceId` per slice, never the `lineKey`). Bundle-component sub-lines, where this gear materializes them, are rating-internal sub-coordinates under the plan line's key — the Billing handoff aggregates to the inherited fact key. Shared **identically** with the Subscriptions period-fact key (SUB-D-19/21), and since T-D-33 the period coordinate is shared too (`AnchorPeriod` ≡ the fact's period identity) — the two gears' period keys no longer differ at all.
- **Obligations** — `TrueUpObligation` and `PeriodFloorCapObligation` value objects (shapes owned by slices 05/09; the Foundation defines only their emission envelope).

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-evaluation-core-fnd`

- **`EvaluationPipeline`** — composes the registered step evaluators in the fixed §17.1 order; short-circuits fail-closed on any step error.
- **`ScopeKeyAdapter`** — materializes the adopted 8-axis key from context + snapshot (§4.1): full-key window selection, eligibility class order, cohort-by-pinned-price-id.
- **`SnapshotComposer`** — verifies the pricing pre-stamp + Subscriptions binding, appends the eval-time segments, seals the ref (§4.3).
- **`DeterminismGuard`** — derives the usage/correction idempotency keys, enforces partition-key serialization for re-resolve, and stamps the frozen-input digest into metadata so replay divergence is detectable (§4.2).
- **`EmissionGuard`** — non-negative clamp/credit, full-precision emission, rounding-policy id record, obligation envelope (§4.4).
- **`MetadataRecorder`** — evaluation metadata + discount lineage (pre/post overlay, pre/post coupon, applied ids) for Billing/Tax gross-vs-net and audit.

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-evaluate-fnd`

The **evaluation contract** (conceptual; consumed by Rating in-process): `evaluate(EvaluationContext) → ResolvedPriceOutcome`. Replay-safe; same context + same frozen inputs ⇒ byte-identical outcome. Failure modes are fail-closed problem values (no eligible window on the full key; injectivity violation; missing `periodState` / coupon policy / FX record; snapshot integrity failure) — the concrete error taxonomy is defined in this Design set, not the PRD.

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-interface-reresolve-fnd`

The **re-resolution contract**: `reresolve(window, pinnedSnapshotRef, priorRatedVersion) → deltas`. Strictly snapshot-replayed (no live catalog read); carries the correction key; emits only deltas.

External integration contracts (Rating handoff, pricing read-model input, Subscriptions input, Finance/Promotions/Billing) are owned by [`11-consumer-contracts.md`](./11-consumer-contracts.md).

### 3.4 Internal Dependencies

None upstream — this is the root slice. Slices 02–07 register step evaluators; 08–09 wrap the
per-line flow (retroactivity, period obligations); 10 registers publish validators
(pricing-side pipeline, not here); 11 formalizes the boundary contracts.

### 3.5 External Dependencies

| Dependency | What arrives frozen | Contract |
|------------|--------------------|----------|
| Pricing (Product Catalog) | pinned read model (key, windows, model kinds, bands, enums), `PriceWindow*` + `CatalogVersionPublished` events | [`11-consumer-contracts.md`](./11-consumer-contracts.md); SEAMS C1 |
| Rating pipeline (intra-gear since T-D-16 — slices 12/13/15, listed for the boundary, not as an external system) | windowed `Q` (single-writer per partition key), usage dedup | PRD §9.2 handoff; slices 12–15 |
| Subscriptions | `phase_id`, eligibility inputs (`activatedAt`, bound cohort), seat count, `(changeEffectiveAt, changeMode)` | PRD §9.2 Subscriptions input |
| Finance | FX tables + lock policy (`fxTableVersion`) | PRD §9.2 Finance FX |
| Promotions | frozen coupon snapshots | PRD §9.2 Promotions |
| Billing | `periodState`; executes obligations + rounding | PRD §9.2 Billing |

### 3.6 Interactions and Sequences

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-flow-evaluate-line-fnd`

**Evaluate one line** (implements `cpt-cf-bss-rating-seq-evaluate-tariff`):

1. Assemble `EvaluationContext`; verify every frozen input is present (fail closed otherwise).
2. Steps 1–2 (slice 02): resolve `phase_id`; select the single window on the full 8-axis key; eligibility class order; cohort by pinned price id.
3. Step 3 (slice 03): map `(meter, dimensionKey)` injectively; granularity round-up on the merged measure; model formula over the evaluation unit.
4. Steps 4–5 (slice 04): stack scope-matching PriceOverlays (class order breaks ties); apply contract overlay; enforce the anti-drift cap.
5. Step 6 (slice 05): reservation match first — a consumption split re-runs steps 3–5 as a unit over the on-demand remainder (T-D-13) — then commitment-pool waterfall; obligations surfaced, never posted.
6. Step 7 (slice 06): coupons per stacking policy (price-currency before FX).
7. Step 8 (slice 07): FX per policy; record `fxTableVersion`/lock; billing-currency coupons after.
8. Step 9 (Foundation): `EmissionGuard` (non-negative → full precision → rounding-policy id), `SnapshotComposer` seals the ref, `MetadataRecorder` attaches lineage; hand the outcome to Rating.

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-reresolve-open-window-fnd`

**Open-period re-resolution** (implements `cpt-cf-bss-rating-seq-open-period-reresolve`):

1. Late/corrected usage lands in Rating; the window's `Q` is re-materialized (Rating, single-writer).
2. `reresolve` replays the **pinned** snapshot for the whole window unit (no live read; serialized on the partition key).
3. Diff against `prior-rated-version` → deltas only, keyed `(unitKey[, slice], prior-rated-version, snapshot)` (§4.2).
4. `periodState = closed_posted` routes the same diff through posted-period protection (slice 08) instead.

### 3.7 Database Schemas and Tables

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-storage-none-fnd`

**None owned.** The Foundation holds no authoritative table. The only local state is a
**non-authoritative resolved-window cache** (pinned read-model pages keyed by snapshot ref;
invalidated by `PriceWindowScheduled/Activated/Expired/Cancelled` and `CatalogVersionPublished`)
whose loss degrades latency, never correctness. Authoritative state map: catalog → pricing
gear; `Q`/dedup/rated output → Rating; commitment balances → Contracts; FX → Finance; coupon
entities → Promotions; period/rounding → Billing.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-fnd`

The evaluation core is **`rating-core`**, a pure, I/O-free crate inside the one `rating` gear
deployable (ADR-0002 / T-D-16 — this supersedes the earlier placement note that treated the evaluation
core as a logical module of the BSS Rating deployable; the one-deployable, no-separate-evaluation-service constraint is
preserved and strengthened to a compiler-checked crate boundary + CI deny-list). Horizontal
scale is per-partition `(subscription, meter, dimensionKey, window)` with zero cross-partition
locks; the resolved-window cache is per-instance and safely cold-startable.

## 4. Additional Context

### 4.1 Adopted Canonical Scope Key (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-adopted-key-fnd`

Selection and non-overlap use the pricing canonical key **verbatim**:

```text
(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)
```

- "At most one window matches" holds **only on the full key**; coexisting hybrid `chargeKind` rows and grandfathering `cohort` generations are disambiguated by the key, never fail-closed.
- `phase` is a `phase_id` (uuid; kind names are display-only); usage rows are phase-invariant by default, phase-specific wins — the no-gap rule applies to the *resolved* set.
- Eligibility class order: `existing_grandfathered > new_subscriptions_only > all_subscriptions`. Within `existing_grandfathered` the generation is the row whose `cohort` equals the cohort of the subscription's **pinned price id** in `pricingSnapshotRef` — never `activatedAt` alone.
- Definition SoR: pricing `ADR/0001` + `ADR/0002`; adoption rationale: [`../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md`](../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md). Guarded by the joint fixture set (hybrid + multi-generation).

### 4.2 Determinism and Idempotency Contract (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-determinism-fnd`

- **Evaluation unit** — three kinds: `per_event` ⇒ one normalized `UsageRecord`; windowed models ⇒ the aggregated `Q` for `(subscription, meter, dimensionKey, window)` — **one unit per sub-window slice** when a slice-09 split partitions the window, coupled to earlier slices only through the frozen `bandOffsetQ` input ([`03-metering-models.md`](./03-metering-models.md) §4.3, T-D-12); **period-driven** ⇒ recurring lines, capacity-flavor charges, and period-end true-up surfacing, keyed `(subscription, priceId, chargeKind, lineKey, AnchorPeriod)` and synthesized by Rating's **fact-driven** period tick on consumption of the subscriptions period-fact set ([`11-consumer-contracts.md`](./11-consumer-contracts.md) §4.1/§4.3, T-D-15 as re-triggered by T-D-33 — the calendar is geometry/watchdog, never the trigger) — a zero-usage period still produces its period-driven units, by construction (the facts arrive regardless of usage). `Q` and the per-slice attribution are materialized and owned by the **rating pipeline** (slice 13 `QMaterializer`; single writer per partition key); **rating-core** never aggregates (core/pipeline per PRD §2.1 — 2026-07-28 review fix).
- **Frozen tuple**: `(window-aggregated inputs incl. bandOffsetQ for a sub-window slice — and, for a `per_unit` line, the sealed quantity pair `(N, source ref)` (2026-07-31 review #18: the seat count is a determinism input with a replay source, not ambient state) — pricingSnapshotRef, fxTableVersion)` ⇒ byte-identical monetary outcome across replay, recompute, and cross-region workers. Every unit binds exactly **one** pinned snapshot — a split window is several units (one per slice), never one unit over several snapshots (T-D-12). The `DeterminismGuard` stamps a frozen-input digest into metadata; a divergence without an input change is a defect by definition.
- **Serialization**: concurrent re-resolve for one unit serializes on the partition key; there are no cross-partition locks.
- **Keys**: usage idempotency key (Rating dedup authoritative — same key + same snapshot never double-charges); correction key `(unitKey[, slice], prior-rated-version, snapshot)` for every delta (retry replays, never double-adjusts) — **`unitKey` generalizes over both unit families** (2026-07-28 review fix, Blocking #1): the usage counter key `(subscription, meter, dimensionKey, window)` *or* the period-driven key `(subscription, priceId, chargeKind, lineKey, AnchorPeriod)`, exactly as `rated_output` already keys — a close-time FX re-rate of a recurring line, a true-up recompute, and a capacity-charge correction are deltas too and must collide on retry; the slice coordinate is present iff a slice-09 split partitions a usage window (never on period-driven units). Delta-dedup **owner**: **Rating** (§2.2, T-D-11).
- **Snapshot-only replay**: open-period re-resolution and posted-period corrections both replay the pinned snapshot; a live catalog read on a correction path is a defect (SEAMS W2).

### 4.3 pricingSnapshotRef Composition (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-snapshot-composition-fnd`

One ref, **four writers** — registry, pricing, Subscriptions, Rating ("three writers" predated
the D-66 producer split under which `catalogVersion` is the registry's; the table always had
four — 2026-07-31 review, #10) — non-overlapping segments; **Rating is the composition SoR**
and seals the ref at emission — immutable thereafter:

| Segment | Writer | When |
|---------|--------|------|
| `catalogVersion` (pending → committed) | **registry (products gear)** — sole incrementer; pricing only requests addressability (pricing D-66: the registry, never pricing's outbox, emits `CatalogVersionPublished`) | pricing publish → registry `CatalogVersionPublished` |
| resolved price ids (**incl. `cohort`**) | pricing gear | publish |
| evaluation-policy version — **explicitly including the level-aggregation triple** `aggregationFunction`/`aggregationGranularity`/`maxHold` per non-`sum` usage row (D-44/T-D-17; 2026-07-28 review fix — previously only implicit inside the pricing pre-stamp) | pricing gear | publish |
| `(currency, region)` binding | Subscriptions | activation |
| resolved overlay / `priceOverlay` ids | Rating | evaluation |
| applied coupon id(s) + stacking policy | Rating | evaluation |
| FX-lock id (if any) | Rating | evaluation |
| `commitmentReservation` — reservation match (id, flavor, `reservedQuantity`, resolved rate source); pool set (per-pool id, unit, `poolType`, balance-as-of + `balanceVersion`, draw order, rollover, optional **`overageRate`** — the T-D-19 residual-pricing selector, frozen at contract publish; 2026-07-31 review #5: this "identical" list had dropped the money selector); reserved-vs-pool split | Rating | evaluation |

The `SnapshotComposer` rejects (fail-closed) a context whose pricing pre-stamp or Subscriptions
binding is missing or torn; it never fabricates a segment.

**Resolved (2026-07-11, T-D-09)**: the step-6 frozen identifiers form the **eighth named
segment** `commitmentReservation` (row above) — writer Rating @ evaluation, content sourced from
the Contracts-frozen context inputs; no new writer, no pricing-side change. Recorded identically
in [`05-commitments-reservations.md`](./05-commitments-reservations.md) §4.1,
[`11-consumer-contracts.md`](./11-consumer-contracts.md) §4.7, and [`../SEAMS.md`](../SEAMS.md)
S1; its `balanceVersion` is the frozen balance-sequencing point of T-D-10.

### 4.4 Emission Guards (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-emission-guards-fnd`

Applied in order at step 9, per line:

1. **Non-negative**: after step 8 (FX + the billing-currency coupon pass — the last per-line money mutation, and the one that can first drive the line negative via a billing-currency `fixed_amount` coupon) and before any period-level phase, a would-be-negative line is clamped to zero or emitted as a structured credit (clamp-vs-credit policy: PRD §15 open); a floor never masks a negative line.
2. **Full precision**: amounts leave at full intermediate precision (concrete DECIMAL precision fixed in this Design — open with Billing); the pipeline never rounds and never applies period floor/cap — Billing executes both, and the emission records the rounding-policy id.
3. **Lineage**: discount lineage (pre/post-overlay, pre/post-coupon amounts + applied ids) and ASC 606 refs (`performanceObligationRef`, `sspSnapshotPointer` — null at MVP) ride the outcome envelope.

## 5. Traceability

**Traces to**: `cpt-cf-bss-rating-fr-deterministic-evaluation-api`, `cpt-cf-bss-rating-fr-pre-purchase-evaluation`, `cpt-cf-bss-rating-fr-single-outcome-determinism`, `cpt-cf-bss-rating-fr-idempotency`,
`cpt-cf-bss-rating-fr-non-negative-price`, `cpt-cf-bss-rating-fr-separation`, `cpt-cf-bss-rating-fr-evaluation-order`

- **PRD**: §6.1 (all seven FRs above), §6.3 `fr-evaluation-order`, §7.1 NFRs, §17.1 (normative step order), §4.1 (environment constraints).
- **Seams**: K1/K3 (key), S1 (snapshot), M7 (counter key), W2 (snapshot replay), M11 (catalog guarantees) — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-01, T-D-03, T-D-04 — [`../DECISIONS.md`](../DECISIONS.md).
- **ADR**: [`../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md`](../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md).
- **Step slices**: [`02-selection-eligibility.md`](./02-selection-eligibility.md) … [`07-currency-fx.md`](./07-currency-fx.md) register evaluators; [`08`](./08-retroactivity-corrections.md)/[`09`](./09-period-plan-change.md) wrap; [`11-consumer-contracts.md`](./11-consumer-contracts.md) owns the boundary.
