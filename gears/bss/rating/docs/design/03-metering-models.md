Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Metering & Pricing Models (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Pricing (Product Catalog), Rating, Subscriptions, OSS Metering | Downstream: Rating | Owners: BSS Rating team -->

# DESIGN — Metering & Pricing Models (Slice 3)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-design-metering-models`

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
  - [4.1 Model Formulas (normative)](#41-model-formulas-normative)
  - [4.2 Meter Mapping and Dimensional Lines (normative)](#42-meter-mapping-and-dimensional-lines-normative)
  - [4.3 Tier Aggregation Window and `Q` (normative)](#43-tier-aggregation-window-and-q-normative)
  - [4.4 Granularity Round-Up (normative)](#44-granularity-round-up-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

Metering & Pricing Models is the **step 3 evaluator** plus the **model-formula library**: it
maps the evaluation unit to a charge line keyed `(meter, dimensionKey)` (injective per plan
revision, fail-closed otherwise), normalizes the measure (`billingGranularity` round-up on the
**merged** aggregate, never per raw record), resolves the tier-counter window, and computes the
model math for the catalog `modelKind` set `{flat, per_unit, graduated, volume, package}` —
the pricing §17.2 **kind→formula mapping adopted verbatim as shared SoR** (SEAMS M1). `hybrid`
and `committed` are **not** model kinds: a hybrid plan is a composition emitting two lines
under one `planId`; committed usage is a commitment pool over a base model, evaluated at step 6
([`05-commitments-reservations.md`](./05-commitments-reservations.md)).

The slice also owns the two cloud-defining launch capabilities riding step 3: **dimensional
lines** — each distinct `(meter, dimensionKey)` prices as its own line, with the declared
dimension set frozen in `pricingSnapshotRef` and no silent collapsing of partial values — and
**composite (derived) meter evaluation** — a catalog-declared frozen formula-as-data over ≥ 2
input units producing one output quantity that is then priced by its own `modelKind`
(SEAMS M5). What it does **not** own: `Q` aggregation (Rating, single-writer per
`(subscription, meter, dimensionKey, window)` — SEAMS M7), dimension **declaration** authoring
(pricing catalog) and dimension **value emission** (OSS metering, the external critical path),
pool drawdown (slice 05), and period floor/cap (slice 09; bands are open-top by catalog
guarantee — capping is a period-level obligation).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-fr-meter-mapping-granularity` | `MeterMapper` maps the unit to `(meter, dimensionKey)` injectively per plan revision (configuration error ⇒ fail closed); `GranularityNormalizer` rounds up the merged measure exactly once (§4.4). |
| `cpt-cf-bss-rating-fr-flat-pricing` | `ModelFormulaEvaluator`: `unitPrice × Q` (or fixed amount per period for recurring); no thresholds evaluated (§4.1). |
| `cpt-cf-bss-rating-fr-per-unit-pricing` | `per_unit` = `unitPrice × quantity` where quantity comes from the frozen `quantitySource` (`subscription_seat_count` from Subscriptions, or `manual`) — **never** metered `Q`; pricing **p1 launch**, joint fixture (SEAMS M2). |
| `cpt-cf-bss-rating-fr-tiered-graduated` | Marginal band math per band; a single-band graduated is numerically Variant A — distinguished by configured kind, not by math; counter per the resolved window (§4.3). |
| `cpt-cf-bss-rating-fr-volume-variant-a` | One band rate applied to **all** units by total `Q` in the window; explicit per-SKU configuration, distinguishable from graduated (§4.1). |
| `cpt-cf-bss-rating-fr-package-pricing` | `ceil(usedQ / packageSize) × packagePrice` over the window; partial block rounds up to one block; parity with pricing p2 launch, joint fixture (SEAMS M3). |
| `cpt-cf-bss-rating-fr-hybrid-pricing` | Composition, not a kind: two lines under one `planId`, independently evaluated; min-commit expressed as committed-usage, never conflated with a period floor; attachment configuration frozen in `pricingSnapshotRef` (§4.1). |
| `cpt-cf-bss-rating-fr-committed-usage` | Composition over a base model: in-commitment vs overage rates and `TrueUpObligation` are step 6 ([`05`](./05-commitments-reservations.md)); reversal/refill under slice [`08`](./08-retroactivity-corrections.md) keys. This slice contributes only the base-model math the pool wraps. |
| `cpt-cf-bss-rating-fr-dimensional-pricing` | `MeterMapper` prices each distinct `(meter, dimensionKey)` as its own line; empty/partial values on a dimension-declaring plan route to a **published** default/catch-all line or fail closed — never guessed (§4.2). |
| `cpt-cf-bss-rating-fr-dimension-population-contract` | Declaration = catalog; **freeze = this slice** (declared set into `pricingSnapshotRef`); value emission = OSS metering (external critical path); until then `dimensionKey` is the empty tuple (§4.2). |
| `cpt-cf-bss-rating-fr-composite-meter-eval` | `CompositeMeterEvaluator` evaluates the frozen formula-as-data to the output quantity, then prices the output unit by its `modelKind` — **composite inputs are window-`sum` only at launch** (D-44's no-co-occurrence rule: non-`sum` aggregation and composite meters never meet on one row); the pipeline never authors or mutates the derivation (§4.1). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-throughput-latency` | `ModelFormulaEvaluator` | Formula math is O(bands) in-memory over the frozen aggregate; no I/O inside step 3 | Load test; targets provisional (NFR workshop) |
| `cpt-cf-bss-rating-nfr-horizontal-scale` | `Q` consumption | The counter key `(subscription, meter, dimensionKey, window)` **is** the partition key (SEAMS M7) — window math stays partition-local for every single-meter model; the composite meter is the one exception and reads its input `Q`s as frozen values, never live counters (§3.6) | Design + load test |
| `cpt-cf-bss-rating-nfr-resilience` | `MeterMapper` fail-closed | Non-injective mapping, unresolvable dimension routing, or a missing frozen model parameter is a typed configuration failure — never a guessed line | Chaos/retry test + joint fixtures |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-bss-rating-adr-scope-key-adoption` | The window key is the pricing 8-axis key; `(meter, dimensionKey)` keys the **line within** the selected row — it is not a window-selection axis (SEAMS K1 boundary). |
| `cpt-cf-bss-pricing-adr-canonical-scope-key` (adopted) | Key definition SoR; the usage-only restriction for tier models rides the key's `chargeKind` axis (pricing D-18). |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-mm`

```text
Step 3 evaluator (this slice)     MeterMapper · GranularityNormalizer · TierWindowResolver ·
        │  (registers into §17.1 slot 3)      ModelFormulaEvaluator · CompositeMeterEvaluator
        ▼
Foundation mechanisms (01)        EvaluationPipeline · EvaluationUnit shapes · frozen-input digest
        │
        ▼
Frozen inputs                     selected row + model params (slice 02 / snapshot) · windowed Q
                                  (Rating) · seat count (Subscriptions) · declared dimension set +
                                  composite formula (pricing snapshot) · dimensionKey values (OSS)
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | The step 3 evaluator: mapping, normalization, model math, composite evaluation | Rust module in the `rating` gear (rating-core crate) |
| Domain | Line key, normalized measure, band/package/per-unit parameter shapes, window spec | Rust; GTS + Rust domain structs |
| Infrastructure | **None owned** — `Q` and dedup live in Rating; model params arrive in the pinned snapshot | In-process (01 §3.7) |

## 2. Principles and Constraints

### 2.1 Design Principles

#### The kind→formula mapping is shared SoR

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-shared-formula-sor-mm`

The catalog `modelKind` enum and its formula semantics are the pricing §17.2 mapping adopted
verbatim (SEAMS M1); Rating implements, never redefines. Commercial constructs that are *not*
kinds (hybrid, committed) are compositions over kinds — reclassifying them as kinds is the
defect this principle guards against.

#### Price the aggregate, not the record

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-aggregate-not-record-mm`

All windowed math — granularity round-up, tier placement, package blocks, composite sums —
operates on the **window-aggregated, merged measure** (`Q` for
`(subscription, meter, dimensionKey, window)`), never on raw `UsageRecord`s. Rating aggregates
(single-writer); Rating prices the normalized aggregate (SEAMS M7).

#### Never guess a line

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-never-guess-line-mm`

An ambiguous meter mapping, a partial dimension tuple, or a missing frozen parameter routes to
an explicitly **published** default line or fails closed — silent collapsing of dimensional
usage into one line is mispricing by construction ([`../PRD.md`](../PRD.md) §6.7).

### 2.2 Constraints

#### Catalog guarantees relied on

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-catalog-guarantees-mm`

Tier bands are **always open-top** (pricing D-17: no closed top, no above-max fail-closed
branch — capping is a period-level obligation, slice [`09`](./09-period-plan-change.md));
`graduated` / `volume` / `package` are **usage-only** (`chargeKind=usage`, pricing D-18);
`aggregationFunction ∈ {sum (default), peak, time_weighted}` per D-44/T-D-17 (the granule
fold of §4.3 — the 2026-07-28 review deleted the stale "sum-only" wording here, which
contradicted §4.3 and the p1 `fr-level-aggregation` and would have made the two level-billed
launch products unrateable); the one surviving `sum`-only boundary is **composite-meter
inputs** (no co-occurrence with non-`sum`, D-44). The evaluator presupposes these guarantees
and carries no code paths for their violation.

#### Quantity sources are typed

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-typed-quantity-mm`

Metered `Q` (Rating), `subscription_seat_count` (Subscriptions), and `manual` are distinct
frozen quantity sources; `per_unit` never reads `Q`, usage models never read seat count. The
`quantitySource` is frozen in `pricingSnapshotRef` (SEAMS M2).

#### Dimension declaration is not dimension emission

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-declare-vs-emit-mm`

The catalog persists the declared dimension set; this slice **freezes** it in the snapshot;
Rating passes `dimensionKey` through; OSS metering emits values (external upstream, critical
path — [`../PRD.md`](../PRD.md) §16, §17.3). Until OSS emits, `dimensionKey` stays the empty
tuple. Cross-doc wording of the launch posture is the **open seam M6**
([`../SEAMS.md`](../SEAMS.md)).

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-mm`

All value objects; model parameters are frozen snapshot content, never authored here.

- **`ChargeLineKey`** — `(meter, dimensionKey)`; injective per plan revision; the empty tuple for plans declaring no dimensions.
- **`NormalizedMeasure`** — the merged, granularity-rounded quantity of the evaluation unit (01 §4.2); carries the pre-round raw aggregate for lineage.
- **`ModelParams`** — the frozen per-kind parameter set: `unitPrice` (flat/per_unit), open-top marginal bands (graduated/volume A), `packageSize`/`packagePrice` (package), `quantitySource` (per_unit), and the **level-aggregation triple** `aggregationFunction`/`aggregationGranularity`/`maxHold` (non-`sum` usage rows — D-44/T-D-17, §4.3; 2026-07-28 review fix: §4.3 consumed them but this list omitted them).
- **`TierWindowSpec`** — the resolved `tierAggregationWindow` value + concrete UTC boundaries (anchor policy applied); recorded in metadata and frozen in the snapshot.
- **`QCounterRef`** — the window-aggregated `Q` for `(subscription, meter, dimensionKey, window)` — Rating-owned, consumed frozen; never written here.
- **`CompositeFormula`** — the frozen formula-as-data: input unit set (≥ 2), window-`sum` derivation, output unit; catalog-declared (pricing Slice 10).
- **`ModelLineOutcome`** — per-line model result: effective rate(s), band/block placement, quantities in/out, feeding steps 4+ and the outcome lineage.

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-metering-models-mm`

The step 3 evaluator, registered into the fixed slot (01 §3.2):

- **`MeterMapper`** — evaluation unit → `ChargeLineKey`; asserts injectivity per plan revision (violation ⇒ fail-closed configuration error); routes empty/partial dimension tuples on a dimension-declaring plan to the published default/catch-all line or fails closed (§4.2).
- **`GranularityNormalizer`** — applies `billingGranularity` round-up once, on the merged measure (§4.4); records the granularity in metadata.
- **`TierWindowResolver`** — resolves `tierAggregationWindow` to concrete UTC boundaries: `calendar_month` in UTC; `invoice_period` per the frozen `billingAnchorPolicy` + D-20 clamp; `subscription_lifetime`; `per_event` (§4.3).
- **`ModelFormulaEvaluator`** — the five kind formulas (§4.1) over `NormalizedMeasure` / `QCounterRef` and `ModelParams`; full intermediate precision, no rounding (01 §4.4).
- **`CompositeMeterEvaluator`** — evaluates `CompositeFormula` to the output quantity, then delegates the output unit to `ModelFormulaEvaluator` under its own `modelKind` (§4.1).

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-price-line-mm`

The **step 3 contract** (internal; pipeline-invoked): `price_line(SelectionOutcome,
EvaluationUnit, frozen params) → ModelLineOutcome | ModelProblem`. Deterministic over the
frozen tuple (01 §4.2). Problems (this slice's rows in the Design-set error taxonomy):
`non_injective_mapping`, `unroutable_dimension_tuple`, `missing_model_param`,
`unknown_model_kind` (enum drift — guarded upstream by the CI gate, SEAMS P1 pattern),
`max_hold_exceeded` (the D-44 sampling-gap signal: the granule folds to level 0 **and** this
operator signal raises — the outcome still emits, unlike every other row here; 2026-07-28
review fix) — all others fail-closed.

### 3.4 Internal Dependencies

Upstream: [`01-foundation.md`](./01-foundation.md) (pipeline, evaluation unit, precision
guards); [`02-selection-eligibility.md`](./02-selection-eligibility.md) (the selected row and
its `modelKind`/params provenance). Downstream: [`04-overlays-precedence.md`](./04-overlays-precedence.md)
stacks overlays on the model output; [`05-commitments-reservations.md`](./05-commitments-reservations.md)
wraps it in pool/reservation math; [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md)
drives the correction-time counter decrement (Rating executes the decrement; the key is M7's).

### 3.5 External Dependencies

| Dependency | What arrives frozen | Contract |
|------------|--------------------|----------|
| Rating pipeline (intra-gear — slices 12/13) | window-aggregated `Q` per `(subscription, meter, dimensionKey, window)` (single-writer); merged session measures for continuous-duration meters | PRD §9.2 handoff; slices 12/13; SEAMS M7 |
| Pricing (Product Catalog) | `modelKind` + per-kind params, declared dimension set, composite formula, `tierAggregationWindow` / `billingAnchorPolicy` — all in the pinned snapshot | [`11-consumer-contracts.md`](./11-consumer-contracts.md); SEAMS M1/M3/M5 |
| Subscriptions | `subscription_seat_count` for the `per_unit` `quantitySource` | PRD §9.2 Subscriptions input; SEAMS M2 |
| OSS Metering | `dimensionKey` **values** on usage (external critical path; empty tuple until delivered) | PRD §6.7, §17.3; SEAMS M6 (open wording) |

### 3.6 Interactions and Sequences

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-flow-price-line-mm`

**Price one line** (step 3 of `cpt-cf-bss-rating-seq-evaluate-tariff`):

1. `MeterMapper`: evaluation unit → `ChargeLineKey`; injectivity assert; dimension routing (§4.2).
2. `GranularityNormalizer`: round the merged measure up to `billingGranularity` — exactly once (§4.4).
3. `TierWindowResolver`: resolve the window and its UTC boundaries; for windowed models bind `QCounterRef` (§4.3).
4. `ModelFormulaEvaluator`: apply the kind formula (§4.1) at full intermediate precision.
5. Emit `ModelLineOutcome` (+ granularity, window value, band/block placement into metadata); hand to step 4.

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-composite-eval-mm`

**Composite (derived) meter**: evaluate the frozen formula (window-`sum` over ≥ 2 input units)
→ one output quantity → the output unit's line is priced by its own `modelKind` through the
same steps; formula, input set, and output unit are frozen in `pricingSnapshotRef`; Rating
never authors or mutates the derivation (SEAMS M5).

**Composite partition rule**: the input `Q`s come from ≥ 2 *different* meters — different
partition keys — making the composite the one launch case where an evaluation unit reads across
counter partitions. The input `Q`s enter the frozen tuple as ordinary frozen inputs (digested by
the `DeterminismGuard`, 01 §4.2); the composite line itself partitions on
`(subscription, outputUnit, dimensionKey, window)`; a late-arrival change to **any** input `Q`
triggers re-resolution of the composite line under the slice-08 correction keys. Reads are of
frozen values, never live counters — no cross-partition lock exists.

**Composite × dimensions**: at launch the two do not co-occur (`dimensionKey` is the empty tuple
until OSS emission — SEAMS M6). The input-join rule for dimension-carrying inputs (join on the
matching tuple vs a formula-declared join) MUST be pinned jointly with the pricing gear before
both are live — tracked open.

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-dimension-routing-mm`

**Dimension routing**: a record with empty/partial dimension values on a dimension-declaring
plan → the explicitly **published** default/catch-all line if defined, else fail-closed
(reject/quarantine) — never silently priced as a single line ([`../PRD.md`](../PRD.md) §6.7).

### 3.7 Database Schemas and Tables

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-storage-none-mm`

**None owned.** `Q` and usage dedup live in Rating; model parameters, dimension declarations,
and composite formulas live in the pinned catalog snapshot; nothing model-side persists in
Rating (01 §3.7).

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-mm`

Nothing beyond the Foundation posture (01 §3.8). The counter key doubling as the partition key
(SEAMS M7) is what keeps step 3 horizontally scalable — single-meter window math is
partition-local by construction; the composite meter reads its frozen input `Q`s across
partitions without locks (§3.6).

## 4. Additional Context

### 4.1 Model Formulas (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-model-formulas-mm`

The catalog `modelKind` enum and formulas — the pricing §17.2 mapping, shared SoR (SEAMS M1):

| `modelKind` | Formula | Notes |
|-------------|---------|-------|
| `flat` | `unitPrice × Q`, or a fixed amount per period (recurring) | no thresholds evaluated |
| `per_unit` | `unitPrice × quantity` from frozen `quantitySource ∈ {subscription_seat_count, manual}` | **never** metered `Q`; p1 launch (SEAMS M2) |
| `graduated` | marginal band rate per unit within each open-top band | single-band case numerically = volume A; distinguished by configured kind |
| `volume` | one band rate × **all** units by total `Q` in the window | **Variant A only**; Variant B (per-tier block fee) is **not authorable** (pricing D Q3, SEAMS M4) |
| `package` | `ceil(usedQ / packageSize) × packagePrice` over the window | partial block rounds **up**; p2 launch (SEAMS M3) |

- `hybrid` and `committed` are **compositions**, not kinds: hybrid = two lines (recurring + usage) under one `planId`, independently evaluated per their period boundaries; a hybrid "minimum commitment" is committed-usage (pool + overage, step 6) — **never** conflated with a period floor (slice [`09`](./09-period-plan-change.md)). Attachment points (commitment/floor to the usage line unless plan-level; coupon per `applyScope`, `line_total` split back pro-rata deterministically — executed by slice [`06`](./06-coupons.md)) are **frozen in `pricingSnapshotRef`**.
- `graduated`/`volume`/`package` are usage-only (D-18); bands are open-top (D-17); launch aggregation is `aggregationFunction ∈ {sum, peak, time_weighted}` per the SEAMS M10 re-scope (T-D-17; §4.3) — *the pre-re-scope "sum only" limit is void, and this line had kept it after §2.2 and §4.3 were fixed (2026-07-31 billing-domain review, #1)*.
- **Band boundary rule** (money-affecting, adopted): thresholds are half-open `[lower, upper)` — a quantity exactly at a boundary falls in the **upper** band, identically for graduated marginal placement and volume-A band selection ([`../PRD.md`](../PRD.md) §1.4 Tier aggregation window).
- A free-tier allowance in current scope is expressed as a per-`(meter, dimensionKey)` **$0 band** ([`../PRD.md`](../PRD.md) §15); a cross-account allowance is a Follow-on aggregate.

### 4.2 Meter Mapping and Dimensional Lines (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-dimensional-mapping-mm`

- The mapping unit → `(meter, dimensionKey)` MUST be **injective per plan revision**; violation is a fail-closed configuration error, never a merged line.
- Each distinct `(meter, dimensionKey)` resolves to **its own** charge line and price; a plan declaring no dimensions prices as the single empty-tuple line.
- Empty/partial dimension values on a dimension-declaring plan route to an explicitly **published** default/catch-all line, else fail closed (reject/quarantine) — never guess.
- Ownership split (SEAMS M6, PRD §6.7): catalog **declares** (persists `dimension_key` structurally now); this slice **freezes** the declared set in `pricingSnapshotRef`; Rating passes values through; OSS metering **emits** values — the external critical path (§17.3). Until emission lands, `dimensionKey` is the empty tuple and per-combination meters are the only workaround (cardinality risk, §16). **Closed 2026-07-28:** the cross-doc launch-posture wording (seam M6) is now stated identically on both sides — *declaration + freeze are in scope now (the catalog persists `dimension_key` structurally, this gear freezes the declared set in the snapshot); pricing dimension **values** are OSS-emission-gated* (pricing design/03 §6 carries the same sentence).

### 4.3 Tier Aggregation Window and `Q` (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-tier-window-mm`

- `tierAggregationWindow ∈ {calendar_month, invoice_period, subscription_lifetime, per_event}` governs when the tier counter resets; the active value is recorded in evaluation metadata **and** frozen in `pricingSnapshotRef` ([`../PRD.md`](../PRD.md) §6.5).
- Boundaries: `calendar_month` in UTC; `invoice_period` anchored per the frozen catalog `billingAnchorPolicy ∈ {calendar_month, subscription_start, fixed_day(d)}` with the D-20 no-drift clamp (31→28→31, anchor day preserved) — the anchor authority is slice [`09`](./09-period-plan-change.md)'s adopted enum (SEAMS P2); this slice consumes the resolved boundaries.
- The counter is the window-aggregated `Q` for **`(subscription, meter, dimensionKey, window)`** (SEAMS M7 — the superset key: per-subscription reset scope + per-dimension counters); the **rating pipeline** (slice 13 `QMaterializer`) is the single writer per this key; **rating-core** never aggregates and never mutates the counter (core/pipeline per §2.1 — the post-rename "Rating…Rating" collapse fixed 2026-07-28). For `tierAggregationWindow ≠ per_event`, tier/volume/package math evaluates over `Q`; for `per_event`, the unit is the event (01 §4.2).
- **Aggregation derivation (D-44 / T-D-17)**: `Q`'s derivation is the row's frozen `aggregationFunction ∈ {sum (default), peak, time_weighted}`. For `sum`, `Q` is the plain sum of normalized measures. For non-`sum` the meter is **level-shaped** (gauge samples in the level unit): the window is cut into `aggregationGranularity ∈ {hour (default), day}` granules; each granule folds deterministically — `peak` = max sample in the granule, `time_weighted` = step-integral of the level over the granule (`hold_last` bounded by the declared `maxHold` — **an integer count of granules ≥ 1** (pricing design/03 §6, the field's SoR): the last level legitimately carries **across granule boundaries** for up to `maxHold` granules, so granule N+1's integral may depend on N's last sample, and a late/backfilled sample in granule N re-folds **N through N+maxHold** — each re-fold its own standard delta; the "re-folds only its granule" shorthand elsewhere reads *per affected granule*, and the whole-window recompute (slice 13 §4.4) stays governing (2026-07-31 review, #16 — the rating set had never stated `maxHold`'s unit); beyond the hold the level reads 0 + the `max_hold_exceeded` operator signal, never a guess — **the momentary under-billing this implies is the accepted product call** (D-44, 2026-07-28 review #19): zero is deliberately the customer-favourable floor and it is **provisional**, because a late/backfilled sample re-folds only its granule and emits the standard delta (§4.4 lane) — the alternative, quarantining the granule, would block the whole window's rating on a metering blip) — and **`Q` = Σ granule folds**, so `Q` stays **additive** and every rule in this section (band math, supersession continuity, `bandOffsetQ` slice math, package cumulative-ceil) applies unchanged. The billable unit is **level unit × granule duration** — GB·h / cloudlet·h at `hour`, **GB·day at `day`** (pricing design/03 §6 publishes it so, and pricing D-77 pins the pairing `hour ⇒ per_hour` / `day ⇒ per_day` precisely to keep band edges aligned; the flat "level·granule-hours" gloss was exact only at `hour` — 2026-07-31 review, #46) — the SKU-declared unit, distinct from the sample's level unit. A late/corrected sample re-folds **only its granule** → a `Q` delta under the standard re-materialization (slice [`13`](./13-q-store-attribution.md) §4.4). Non-`sum` does not co-occur with composite meters at launch ([`../PRD.md`](../PRD.md) `fr-level-aggregation`).
- **Intra-window boundary continuity (adopted, money-affecting — T-D-12)**: when a slice-[`09`](./09-period-plan-change.md) split point (mid-cycle window activation/supersession, plan change, phase conversion) falls **inside** an open aggregation window, the window remains the counter scope but **each sub-window slice is its own evaluation unit** ([`01-foundation.md`](./01-foundation.md) §4.2): a slice prices only its attributed `Q_slice` under its own single pinned snapshot, and the accumulated prior-slice quantity arrives as the explicit frozen input **`bandOffsetQ`**. Adopted verbatim from pricing: supersession does **NOT** reset an in-window counter — the new row's bands apply to the continued `Q` ([`../../../pricing/docs/design/03-price-structure.md`](../../../pricing/docs/design/03-price-structure.md), joint fixture `inst-tb-window-continuity`). Per-kind slice math:
  - `graduated` — `Q_slice` places marginally into the slice row's bands over `[bandOffsetQ, bandOffsetQ + Q_slice)`;
  - `volume` — every slice's band is selected by the **window-total `Q`** (the full window's cumulative as of the evaluation; for the newest slice that equals `bandOffsetQ + Q_slice`, and an earlier slice NEVER keeps its own partial cumulative as the selection input — volume-A is one rate for **all** units by the total, §4.1); the slice bills `Q_slice ×` the selected band rate of **its own** frozen row; whenever window growth moves the total into a new band, **every already-priced slice re-resolves as a delta** from its own pin to the new total's band (slice [`08`](./08-retroactivity-corrections.md) cascade), so at any settled point all slices of the window price at the single band of the same window total;
  - `package` — blocks are counted **once over the window** by cumulative ceil-diff: the slice bills `ceil((bandOffsetQ + Q_slice)/packageSize) − ceil(bandOffsetQ/packageSize)` blocks at its own frozen `packagePrice` — a straddling block belongs to the slice that opened it, and the window total is exactly `ceil(windowQ/packageSize)`.
  - **Granule × slice boundary (T-D-26, 2026-08-01)** — a granule straddling a slice cut **belongs to the slice that opened it** (the same precedent as the straddling package block above): folds stay whole-granule (slice 13 §4.4 partials), so `Q = Σ granule folds` stays additive — per-slice `peak` maxima over a cut granule are **never** both counted (splitting a granule would make the per-slice maxima sum exceed the whole-granule max, and split points are arbitrary UTC instants).

  Boundary kinds: window-activation/supersession and phase-conversion boundaries **always carry** (`bandOffsetQ` = accumulated prior-slice `Q`; continuity is not configurable); only a **plan-change** boundary consults the snapshot-frozen carry-vs-reset flag routed by slice [`09`](./09-period-plan-change.md) (`reset` ⇒ `bandOffsetQ = 0`). A **collapsed cut** — a plan-change boundary coinciding with a window-activation/phase-conversion instant (slice 09 §4.1 "coincident boundaries collapse into one cut") — consults the **plan-change flag**: it is the only configurable rule at the cut, and the activation/conversion halves carry unconditionally either way (2026-07-31 review, #37). Rating (single-writer) materializes the per-slice attribution and `bandOffsetQ` from event-time attribution; a change to an earlier slice's `Q` shifts later slices' `bandOffsetQ` and re-resolves them under the slice-08 cascade.
- **Reservation remainder re-band (T-D-13; band axis pinned by T-D-23, 2026-08-01 — flagged for veto)**: the consumption-flavor matched quantity is excluded from `Q` (slice [`05`](./05-commitments-reservations.md) §4.2); this slice's band math re-runs over the on-demand remainder as part of the steps-3–5 remainder re-run — band placement first, then the steps-4–5 overlays re-apply to the re-banded amount ([`04-overlays-precedence.md`](./04-overlays-precedence.md) §4.2). **"From zero" means the remainder's band axis starts at the window origin with reserved quantity excluded — never a per-slice reset**: on a reservation-carrying line the banded axis is the **cumulative post-reservation remainder** (T-D-19's basis), maintained window-cumulatively, so a sub-window slice's remainder places over `[remainderOffsetQ, remainderOffsetQ + slice_remainder)` where `remainderOffsetQ` is the window's accumulated prior-slice remainder — slice [`13`](./13-q-store-attribution.md) §4.3 materializes it beside `bandOffsetQ`, whose raw-`Q` definition **excludes reservation-matched quantity on the band axis of such lines**. Before T-D-23, T-D-12's `[bandOffsetQ, …)` placement and this bullet's "from zero" gave two different band placements for a slice carrying both a `reservationMatch` and a non-zero offset, with no precedence stated — five-figure divergence per line on the worked-example bands. A per-slice reset is rejected for the T-D-12 reason: every split would re-enter band 1. The joint fixture set gains the **reservation + sub-window slice** scenario (slice 05 §4.6 list).
- Correction-time counter decrement is executed by Rating under slice [`08`](./08-retroactivity-corrections.md) keys.

### 4.4 Granularity Round-Up (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-granularity-mm`

- `billingGranularity ∈ {per_second, per_minute, per_hour, per_day, whole-unit}`; round-up applies to the **aggregated/merged measure** of the evaluation unit, **never per raw `UsageRecord`** — twelve 5-minute samples at `per_hour` bill **1 hour**, not 12.
- Continuous-duration meters: contiguous usage is merged into a session/window measure first (Rating owns the merge), then rounded up **once**; discrete/`per_event` meters: the unit is the event; windowed models: round-up applies to the window measure **before** tier placement.
- The applied `billingGranularity` is recorded in evaluation metadata; amounts continue at full intermediate precision (01 §4.4).
- A per-resource `minimumCharge` MAY be configured to bound ephemeral-churn over-charge — the churn policy itself is a PRD §15 open (Product + Finance); this slice only honors a configured value.

## 5. Traceability

**Traces to**: `cpt-cf-bss-rating-fr-flat-pricing`, `cpt-cf-bss-rating-fr-per-unit-pricing`, `cpt-cf-bss-rating-fr-tiered-graduated`,
`cpt-cf-bss-rating-fr-volume-variant-a`, `cpt-cf-bss-rating-fr-package-pricing`, `cpt-cf-bss-rating-fr-level-aggregation`,
`cpt-cf-bss-rating-fr-hybrid-pricing`, `cpt-cf-bss-rating-fr-meter-mapping-granularity`, `cpt-cf-bss-rating-fr-tier-aggregation-window`,
`cpt-cf-bss-rating-fr-billing-granularity`, `cpt-cf-bss-rating-fr-dimensional-pricing`, `cpt-cf-bss-rating-fr-composite-meter-eval`

- **PRD**: §6.2 (all seven model FRs incl. hybrid/committed composition status), §6.3 `fr-meter-mapping-granularity`, §6.5 `fr-tier-aggregation-window` + `fr-billing-granularity`, §6.7 (dimensional, dimension-population, composite), §17.1 step 3, §17.2 (kind→formula SoR), §17.3 (cloud phasing).
- **Seams**: M1 (enum/mapping), M2 (per_unit launch-blocking), M3 (package), M4 (Variant B deleted), M5 (composite in launch), M6 (**open** — dimensional wording), M7 (counter key), M10 (re-scoped 2026-07-16: `aggregationFunction {sum, peak, time_weighted}` in launch — pricing D-44 / T-D-17), M11 (D-17/D-18 guarantees) — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-04 (M7 key), T-D-05 (model set + launch scope), T-D-12 (intra-window boundary continuity / `bandOffsetQ`), T-D-13 (steps-3–5 remainder re-run) — [`../DECISIONS.md`](../DECISIONS.md).
- **ADR**: [`../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md`](../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md) (line key vs window key boundary).
- **Related slices**: [`01-foundation.md`](./01-foundation.md) (unit, precision, digest), [`02-selection-eligibility.md`](./02-selection-eligibility.md) (selected row in), [`04-overlays-precedence.md`](./04-overlays-precedence.md) (stack on the model output), [`05-commitments-reservations.md`](./05-commitments-reservations.md) (committed composition), [`06-coupons.md`](./06-coupons.md) (hybrid `applyScope` split execution), [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) (counter decrement), [`09-period-plan-change.md`](./09-period-plan-change.md) (anchor authority, floor/cap).
