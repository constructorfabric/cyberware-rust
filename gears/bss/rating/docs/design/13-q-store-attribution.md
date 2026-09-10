Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Windowed Q Store & Attribution (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: 12-usage-ingestion-normalization | Downstream: 14-unit-synthesis-period-tick, rating-core | Owners: BSS Rating team -->

# DESIGN — Windowed Q Store & Attribution (Slice 13, pipeline)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-design-q-store`

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
  - [4.1 The Windowed Counter and Its Key (normative)](#41-the-windowed-counter-and-its-key-normative)
  - [4.2 Single-Writer Serialization and Q Versioning (normative)](#42-single-writer-serialization-and-q-versioning-normative)
  - [4.3 Per-Slice Attribution and bandOffsetQ (normative)](#43-per-slice-attribution-and-bandoffsetq-normative)
  - [4.4 Re-Materialization and Reversal Decrement (normative)](#44-re-materialization-and-reversal-decrement-normative)
  - [4.5 Composite Input-Q Assembly (normative)](#45-composite-input-q-assembly-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

The **single writer** of the windowed tier counter `Q` per
`(subscription, meter, dimensionKey, window)` (SEAMS **M7** — the counter key doubles as the
partition key). It materializes `Q` from the normalized `UsageRecord`s of slice
[`12`](./12-usage-ingestion-normalization.md); maintains the **per-slice attribution and
`bandOffsetQ`** when a period split partitions an aggregation window (T-D-12); re-materializes `Q`
on late/corrected usage before core replay; realizes the **reversal counter decrement** (the core
never writes a counter); and assembles mutually consistent input-`Q` tuples for composite meters.

The store is the load-bearing reason the core can be a pure function: it converts an unbounded
stream of raw usage into a small set of **frozen, versioned** aggregate values the core prices
deterministically. Two invariants make that safe — **exactly one writer per counter key** (so no
interleaving corrupts an aggregate) and **an explicit `qVersion`** on every counter (so the frozen
determinism tuple binds a *specific* `Q`, and a re-materialization is a new version that triggers
re-resolution rather than a silent mutation). The core aggregates nothing and mutates no counter
([`../PRD.md`](../PRD.md) §9.2, §6.5; core slice [`01`](./01-foundation.md) §4.2).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| Windowed `Q` per `(subscription, meter, dimensionKey, window)` (SEAMS M7, T-D-04) | `QMaterializer` is the **single writer** per key; `Q` is the window-aggregated measure the core's windowed models price; the key **is** the partition key — window math stays partition-local (§4.1). |
| Per-slice attribution + `bandOffsetQ` (T-D-12) | When a slice-[`09`](./09-period-plan-change.md) split partitions an open window, `SliceAttributor` attributes each sub-window's `Q_slice` by event time and derives the frozen `bandOffsetQ` (accumulated prior-slice `Q`) the core consumes (§4.3). |
| Late/corrected usage re-materialization (PRD §6.10) | `QMaterializer` recomputes the affected window `Q` (and the per-slice attribution) from the current `UsageRecord` set, bumps `qVersion`, and signals slice [`14`](./14-unit-synthesis-period-tick.md) to route re-resolution; the decrement of a reversal is realized here, not by the core (§4.4). |
| Composite input-`Q` assembly (core slice [`03`](./03-metering-models.md) §3.6) | `CompositeAssembler` reads the ≥ 2 input-meter `Q`s as a **version-consistent frozen tuple** across partitions (§4.5); the input-join rule for dimension-carrying inputs stays a tracked open. |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-horizontal-scale` | Single-writer partition contract | One writer per `(subscription, meter, dimensionKey, window)`; **zero cross-partition locks**; unrelated counters materialize in parallel; a large tenant sub-shards by hash of `subscriptionId` (§3.8) | Design + load test (slice [`16`](./16-billing-handoff-operations.md)) |
| `cpt-cf-bss-rating-nfr-throughput-latency` | Incremental materialization | The common path is an incremental counter increment keyed by the partition; full re-materialization is the off-hot-path correction case only | Load test |
| `cpt-cf-bss-rating-nfr-resilience` | `qVersion` + idempotent increment | A re-delivered `UsageRecord` (already deduped in slice 12) never double-increments; a crashed materializer resumes deterministically from the `UsageRecord` set; the frozen `qVersion` makes replay detectable | Chaos/retry test |

#### Key Decisions

| Decision | Summary |
|----------|---------|
| `qVersion` per counter | Every counter carries a monotonic `qVersion`; slice [`14`](./14-unit-synthesis-period-tick.md) freezes a specific `qVersion` into the determinism tuple, so a re-materialization is a new version + a re-resolution trigger, never an in-place change to an already-rated aggregate (T-D-04 replay discipline). |
| Window identity is derived, not authored | The `window` coordinate is computed from event time under the subscription's **frozen** `tierAggregationWindow` + `billingAnchorPolicy` (D-20 clamp) using the same `AnchorCalendar` math as core slice [`09`](./09-period-plan-change.md); the store applies the calendar, never invents boundaries. |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-qst`

```text
Normalized usage (slice 12)     UsageRecords (new + correcting), deduped, canonical
        ▼
Q store (this slice)            QMaterializer · WindowResolver · SliceAttributor ·
        │  (single writer / key)  CompositeAssembler · ReversalDecrementer
        ▼
Frozen inputs to the core       versioned Q + bandOffsetQ (per unit), via slice 14 context assembly
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | Counter materialization, window resolution, per-slice attribution, composite assembly, reversal decrement | Rust modules in the `rating` gear (pipeline crate; **not** `rating-core`) |
| Domain | Counter, per-slice attribution, `bandOffsetQ`, `qVersion`, composite input-tuple shapes | Rust; GTS + Rust domain structs |
| Infrastructure | The **`Q` store** (windowed counters + per-slice attribution rows + `qVersion`) | PostgreSQL, SecureORM (`toolkit-db`) |

## 2. Principles and Constraints

### 2.1 Design Principles

#### One writer per counter key

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-single-writer-qst`

Exactly one writer materializes `Q` for a given `(subscription, meter, dimensionKey, window)` — the
load-bearing invariant already normative in the core (slice [`01`](./01-foundation.md) §4.2; PRD
§7.1). No interleaving of increments across writers is possible, so no lock and no cross-partition
coordination is needed on the counter path.

#### Frozen and versioned, never silently mutated

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-versioned-q-qst`

A counter the core rated was rated at a specific `qVersion`; a later change re-materializes to a
**new** version and triggers re-resolution (slice [`08`](./08-retroactivity-corrections.md)) — the
already-rated `Q` value is never edited under the core's feet. Determinism depends on the frozen
tuple binding one `qVersion` (§4.2).

#### Attribute by event time, price by slice

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-attribute-by-time-qst`

When a window is split, `Q` is attributed to sub-window slices by **event time**, and each slice's
`bandOffsetQ` is the accumulated prior-slice `Q`; the core prices each slice as its own unit over
its own pin (T-D-12). Attribution is the store's; band math is the core's (§4.3).

### 2.2 Constraints

#### The core never writes Q

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-core-never-writes-qst`

The evaluation core aggregates nothing and mutates no counter; the reversal decrement, the
re-materialization, and the per-slice attribution are all this slice's (PRD §9.2; core slice
[`03`](./03-metering-models.md) §4.3). The core reads `Q` frozen.

#### Window identity from the frozen calendar

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-window-from-calendar-qst`

The `window` coordinate derives from event time under the subscription's frozen
`tierAggregationWindow` + `billingAnchorPolicy` (D-20 clamp) via the core slice
[`09`](./09-period-plan-change.md) `AnchorCalendar` math; the store never authors a boundary and
never uses a live/mutable anchor.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-qst`

- **`WindowedCounter`** — `(subscription, meter, dimensionKey, window)` → aggregated `Q` + `qVersion` (monotonic) + the contributing `usageKey` set (for deterministic re-materialization).
- **`WindowCoordinate`** — the resolved window identity: `tierAggregationWindow` kind + concrete half-open UTC `[from, to)` boundaries from the frozen anchor (D-20).
- **`SliceAttribution`** — for a split window: per sub-window `[from, to)` → `Q_slice` + the frozen `bandOffsetQ` (accumulated prior-slice `Q`); the ordered slice list of the window.
- **`CompositeInputTuple`** — for a composite meter: the ≥ 2 input-meter `Q`s read at a mutually consistent `qVersion` set, plus the assembly watermark (§4.5).
- **`ReMaterializationSignal`** — emitted to slice [`14`](./14-unit-synthesis-period-tick.md) when a counter or attribution changes: the affected units + the new `qVersion` + the correction lineage.

### 3.2 Component Model

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-q-store`

- **`QMaterializer`** — the single-writer counter path: increments (new usage) or recomputes (correction) `Q` for a key from the `UsageRecord` set; bumps `qVersion` on any change.
- **`WindowResolver`** — computes the `WindowCoordinate` for a `UsageRecord` from event time under the frozen anchor (`AnchorCalendar` math reused from core slice [`09`](./09-period-plan-change.md)).
- **`SliceAttributor`** — maintains `SliceAttribution` + `bandOffsetQ` when a period split partitions the window (T-D-12); recomputes offsets when an earlier slice's `Q` changes (§4.3).
- **`CompositeAssembler`** — reads the version-consistent input-`Q` tuple for a composite meter across partitions (§4.5).
- **`ReversalDecrementer`** — realizes the counter decrement for a correcting/negative `UsageRecord` by recomputing the window `Q` (§4.4).

### 3.3 API Contracts

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-interface-q-store-qst`

**Inbound (from slice [`12`](./12-usage-ingestion-normalization.md))**: normalized `UsageRecord`s
(new + correcting) to count. **Outbound (to slice [`14`](./14-unit-synthesis-period-tick.md))**: the
frozen `(Q, qVersion)` and, for split windows, the per-slice `(Q_slice, bandOffsetQ)`; plus
`ReMaterializationSignal`s that drive cascade routing. The core reads these only through the frozen
`EvaluationContext` slice 14 assembles — never a direct counter read on the hot path.

### 3.4 Internal Dependencies

Upstream: slice [`12`](./12-usage-ingestion-normalization.md) (the `UsageRecord`s counted).
Downstream: slice [`14`](./14-unit-synthesis-period-tick.md) (freezes `Q`/`qVersion`/`bandOffsetQ`
into the context and routes cascades), core slice [`03`](./03-metering-models.md) (prices the
windowed aggregate + reads `bandOffsetQ`), core slice [`08`](./08-retroactivity-corrections.md)
(re-resolves on re-materialization). Reuses core slice [`09`](./09-period-plan-change.md)
`AnchorCalendar` as pure window math.

### 3.5 External Dependencies

| Dependency | What arrives frozen | Contract |
|------------|--------------------|----------|
| Subscriptions (via slice 11 context) | `tierAggregationWindow` + `billingAnchorPolicy` per subscription (for window resolution); split boundaries (`changeEffectiveAt`, phase conversions) | slice [`11`](./11-consumer-contracts.md) §4.3; SEAMS P2 |

_No direct external transport — this slice consumes slice 12's output in-process and serves slice 14._

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-materialize-qst`

**Materialize on new usage**:

1. A `UsageRecord` arrives from slice [`12`](./12-usage-ingestion-normalization.md).
2. `WindowResolver` computes its `WindowCoordinate` from event time under the frozen anchor.
3. `QMaterializer` (single writer for the key) folds the record's measure into `Q`, appends the `usageKey` to the contributing set, and bumps `qVersion`.
4. If the window is split, `SliceAttributor` attributes the measure to the owning sub-window slice and updates that slice's `Q_slice` (and, if it shifts a later slice's start offset, its `bandOffsetQ`).
5. The updated `(Q, qVersion)` (and per-slice values) become available to slice [`14`](./14-unit-synthesis-period-tick.md) for context assembly.

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-rematerialize-qst`

**Re-materialize on correction**:

1. A correcting/negative `UsageRecord` (slice [`12`](./12-usage-ingestion-normalization.md) §4.4) references the affected window.
2. `ReversalDecrementer` / `QMaterializer` recompute `Q` (and per-slice `Q_slice`/`bandOffsetQ`) from the **current** `UsageRecord` set — the decrement is realized here — and bump `qVersion` **in the same transaction that enqueues the `ReMaterializationSignal` via the outbox** (§4.4).
3. The `ReMaterializationSignal` (affected units + new `qVersion` + correction lineage) goes to slice [`14`](./14-unit-synthesis-period-tick.md), which routes `reresolve` through the core (slice [`08`](./08-retroactivity-corrections.md)); a `bandOffsetQ` shift cascades to later slices of the same window (§4.3).

### 3.7 Database Schemas and Tables

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-storage-q-qst`

**Owned (partitioned by the pinned `orderingTenantId`, UTC):**

- `windowed_counter` — PK `(subscription, meter, dimensionKey, window)`; columns: aggregated `Q`, `qVersion` (monotonic), contributing-`usageKey` set (or a digest + a link to slice 12's `usage_record`), last-materialized watermark. **Single-writer** per PK.
- `slice_attribution` — for split windows: `(subscription, meter, dimensionKey, window, slice)` → `Q_slice`, `bandOffsetQ`, slice `[from, to)`; ordered per window.

Concrete DDL is Design. The store is authoritative aggregate state (it survives supersession per M7);
no monetary column (the gear computes no money — core slice [`01`](./01-foundation.md) §3.7).

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-qst`

Pipeline crate inside the one `rating` gear deployable. Single-writer ownership is realized by
**partition ownership**: the counter key hashes to a shard, one materializer instance owns a shard
at a time (coordination lease library), so "single writer per key" holds without a per-row lock;
shards are `orderingTenantId`-scoped and sub-sharded by hash of `subscriptionId` for a large tenant.
Unrelated shards materialize fully in parallel — zero cross-partition locks (PRD §7.1).

## 4. Additional Context

### 4.1 The Windowed Counter and Its Key (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-counter-key-qst`

- The counter key is **`(subscription, meter, dimensionKey, window)`** (SEAMS M7, the superset key — per-subscription reset scope + per-dimension counters; T-D-04); it doubles as the **partition key**, so single-meter window math is partition-local by construction.
- The `window` coordinate is derived from event time under the subscription's frozen `tierAggregationWindow ∈ {calendar_month, invoice_period, subscription_lifetime, per_event}` and, for `invoice_period`, the frozen `billingAnchorPolicy` (D-20 no-drift clamp) — the same `AnchorCalendar` math as core slice [`09`](./09-period-plan-change.md); the store never authors a boundary.
- For `per_event` there is no accumulation — the "counter" is the single event's measure; windowed models (`graduated`/`volume`/`package`) require a non-`per_event` window (core slice [`03`](./03-metering-models.md) §4.3).
- **Non-`sum` folds (D-44 / T-D-17)**: for a meter with frozen `aggregationFunction ∈ {peak, time_weighted}`, `QMaterializer` maintains **per-granule partials** under the counter key (granule = the frozen `aggregationGranularity` cut of the window): `peak` keeps the granule max, `time_weighted` the granule step-integral (`hold_last` bounded by `maxHold`, beyond → 0 + operator signal). The window `Q` is the **sum of granule folds** — additive, so the single-writer key, `qVersion` discipline, and slice attribution apply unchanged. A late/corrected sample **re-folds its granule and, for `time_weighted`, the up-to-`maxHold` following granules whose `hold_last` carried its level** (`maxHold` is an integer count of **granules** — pricing design/03 §6; a step-function level legitimately crosses granule boundaries, so granule N+1's integral can depend on N's last sample — slice [`03`](./03-metering-models.md) §4.3, 2026-07-31 review #16) and bumps `qVersion` — the standard §4.4 re-materialization, scoped to the affected granules' contributions. A granule straddling a sub-window slice cut **belongs to the slice that opened it** (T-D-26 — slice [`03`](./03-metering-models.md) §4.3), so folds stay whole-granule and per-slice attribution never splits a fold.
- Supersession does **not** reset an in-window counter (the new catalog row's bands apply to the continued `Q`, pricing `inst-tb-window-continuity`); the counter's scope is the window, not the catalog revision (§4.3).

### 4.2 Single-Writer Serialization and Q Versioning (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-single-writer-qst`

- Exactly **one writer per counter key** (PRD §7.1; core slice [`01`](./01-foundation.md) §4.2): realized by partition ownership (§3.8), so concurrent increments for one key are serialized without a lock and unrelated keys never contend.
- Every counter carries a monotonic **`qVersion`**, bumped on any materialization (increment or recompute). Slice [`14`](./14-unit-synthesis-period-tick.md) freezes a specific `(Q, qVersion)` into the determinism tuple; the core rates that version. A later change produces a **new** `qVersion` and a `ReMaterializationSignal` — never an in-place edit of the value a prior rating observed (the T-D-04 replay discipline realized on the write side).
- Ingestion has already deduped the source events (slice [`12`](./12-usage-ingestion-normalization.md) §4.3), so a re-delivered `UsageRecord` never double-increments; a crashed materializer resumes by recomputing from the contributing `usageKey` set — deterministic, not offset-fragile.

### 4.3 Per-Slice Attribution and bandOffsetQ (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-slice-attribution-qst`

- When a slice-[`09`](./09-period-plan-change.md) split point (window activation/supersession, phase conversion, plan change) falls **inside** an open aggregation window, the window stays the counter scope but each sub-window slice becomes its own evaluation unit (T-D-12; core slice [`01`](./01-foundation.md) §4.2). The store attributes each `UsageRecord`'s measure to the sub-window that contains its **event time** and maintains, per slice, `Q_slice` and the frozen **`bandOffsetQ`** = the accumulated `Q` of all prior slices of the same window — and, **for a reservation-carrying line, additionally `remainderOffsetQ`** = the accumulated prior-slice **post-reservation remainder**, the band axis of such lines (T-D-23; the raw `bandOffsetQ` includes reservation-matched quantity and stays the attribution/lineage figure, but band placement of the on-demand remainder uses the remainder axis — slice [`03`](./03-metering-models.md) §4.3).
- Continuity is not configurable at window-activation/supersession and phase-conversion boundaries (they always carry — `bandOffsetQ` = accumulated prior-slice `Q`); only a **plan-change** boundary consults the snapshot-frozen carry-vs-reset flag (`reset` ⇒ `bandOffsetQ = 0`) routed by slice [`09`](./09-period-plan-change.md) (core slice [`03`](./03-metering-models.md) §4.3).
- A change to an earlier slice's `Q` **shifts** every later slice's `bandOffsetQ`; the store recomputes the offsets in slice order and emits `ReMaterializationSignal`s for the affected later slices, which slice [`14`](./14-unit-synthesis-period-tick.md) routes as the T-D-12 cascade (each later slice re-resolves under its **own** pin — core slice [`08`](./08-retroactivity-corrections.md) §4.4). The cascade is finite and strictly ordered by slice index.

### 4.4 Re-Materialization and Reversal Decrement (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-rematerialize-qst`

- Late or corrected usage recomputes the affected window `Q` (and per-slice attribution) from the **current** `UsageRecord` set — the reversal counter **decrement is realized here**, by recomputation, not by a core write (core slice [`03`](./03-metering-models.md) §4.3, slice [`08`](./08-retroactivity-corrections.md) §4.4). For an archived window the set is served from the cold tier (slice [`12`](./12-usage-ingestion-normalization.md) §3.7 tiering note) — identical recompute, correction-lane latency only.
- Re-materialization bumps `qVersion` and emits a `ReMaterializationSignal` — **the bump and the signal commit in the same transaction, the signal via a transactional outbox** (the sibling discipline stated at slice [`15`](./15-rated-output-balance-effects.md) "same commit as the store write" and slice [`16`](./16-billing-handoff-operations.md) "in the same commit"; 2026-07-31 review, #9: without the coupling a crash between them left a bumped counter with no delivered signal — no reresolve fired and the rated output stayed frozen at a superseded `Q` until an unrelated correction self-healed it). The core's `reresolve` then diffs the new rating against the prior rated version and emits delta-only adjustments (slice [`15`](./15-rated-output-balance-effects.md)). The counter recompute is deterministic — same `UsageRecord` set ⇒ same `Q`.
- Re-materialization is **off the hot path** (correction lane); the normal path is an incremental increment. A window whose `periodState = closed_posted` still re-materializes its `Q`, but the core routes the diff through posted-period protection (delta-only) — the counter recompute is identical either way (slice [`08`](./08-retroactivity-corrections.md) §4.3).

### 4.5 Composite Input-Q Assembly (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-composite-assembly-qst`

- A composite (derived) meter reads its ≥ 2 input-meter `Q`s from **different** partitions; the store assembles them as a **version-consistent frozen tuple** — each input `Q` pinned at a specific `qVersion`, captured at one assembly watermark — so the core reads frozen values, never live counters, and no cross-partition lock is taken (core slice [`03`](./03-metering-models.md) §3.6).
- A later change to **any** input `Q` (new `qVersion`) re-assembles the tuple and re-resolves the composite line under the slice-[`08`](./08-retroactivity-corrections.md) correction keys; the composite line partitions on `(subscription, outputUnit, dimensionKey, window)`.
- **Open**: the input-join rule when composite inputs carry dimension values (join on the matching `dimensionKey` tuple vs a formula-declared join) MUST be pinned jointly with the pricing gear before composite and dimensional pricing co-occur — tracked in core slice [`03`](./03-metering-models.md) §3.6 / [`../SEAMS.md`](../SEAMS.md); at launch they do not co-occur (`dimensionKey` empty until OSS emission).

## 5. Traceability

- **PRD**: §9.2 (windowed `Q` duties), §6.5 (`tierAggregationWindow`), §6.10 (re-materialization on correction), §17.1 "Determinism and Rating compatibility", §7.1 (single-writer / horizontal-scale NFR).
- **Seams**: M7 (writer side — the counter/partition key) — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-04 (counter key + snapshot-replay), T-D-12 (per-slice attribution + `bandOffsetQ`), T-D-16 (consolidation) — [`../DECISIONS.md`](../DECISIONS.md).
- **ADR**: [`../ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md`](../ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md).
- **Related slices**: [`12-usage-ingestion-normalization.md`](./12-usage-ingestion-normalization.md) (feeds records), [`14-unit-synthesis-period-tick.md`](./14-unit-synthesis-period-tick.md) (freezes `Q`/routes cascades), [`03-metering-models.md`](./03-metering-models.md) §4.3 (band-offset math), [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) (re-materialization + cascades), [`09-period-plan-change.md`](./09-period-plan-change.md) (`AnchorCalendar`, split boundaries).
