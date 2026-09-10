Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Unit Synthesis & Period Tick (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: 12/13 (pipeline), Pricing, Subscriptions, Finance, Promotions, Billing, Contracts | Downstream: rating-core, 15-rated-output-balance-effects | Owners: BSS Rating team -->

# DESIGN — Unit Synthesis & Period Tick (Slice 14, pipeline)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-design-unit-synthesis`

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
  - [4.1 The Three Evaluation-Unit Kinds (normative)](#41-the-three-evaluation-unit-kinds-normative)
  - [4.2 The Period Tick (normative)](#42-the-period-tick-normative)
  - [4.3 Frozen-Context Assembly and Pin Discipline (normative)](#43-frozen-context-assembly-and-pin-discipline-normative)
  - [4.4 Cascade Routing, Coalescing, and Bounding (normative)](#44-cascade-routing-coalescing-and-bounding-normative)
  - [4.5 Commitment-Balance Freezing and the Hot Path (normative)](#45-commitment-balance-freezing-and-the-hot-path-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

The **conductor** between the operational stores and the pure core: it synthesizes the **three
evaluation-unit kinds** (`per_event`; windowed `Q` per sub-window slice; **period-driven** units for
recurring lines, capacity-flavor charges, and true-up surfacing), assembles a fully **frozen**
`EvaluationContext` for each, and invokes the core's `evaluate()` / `reresolve()`. It owns the
**period tick** (T-D-15): at every `AnchorPeriod` boundary it emits the period-driven units — a
zero-usage period still bills its capacity and recurring lines. And it owns **cascade routing**: a
balance-affecting correction (T-D-10) or a `bandOffsetQ` shift (T-D-12) re-enters the core as
delta-only `reresolve`, coalesced and bounded on a correction lane.

This slice is the one place the whole gear's frozen-input discipline is *enforced*: nothing reaches
`rating-core` except through a context this slice sealed — read-model pinned to one committed
`CatalogVersion`, `Q` pinned to a `qVersion`, commitment balances pinned to a `balanceVersion`, FX
to an `fxTableVersion`, coupons to a snapshot, `periodState` from Billing. A missing input fails
closed **at this boundary**, never inside the core (core slice [`01`](./01-foundation.md) §2.1, slice
[`11`](./11-consumer-contracts.md) §2.1). Because balances are **frozen into the context**, the core
hot path is partition-local and never waits on a live cross-unit balance — the cross-unit ordering
lives on the asynchronous write-back (slice [`15`](./15-rated-output-balance-effects.md)), off the
hot path (§4.5).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| Three unit kinds (core slice [`01`](./01-foundation.md) §4.2) | `UnitSynthesizer` produces `per_event` units (one `UsageRecord`), windowed-`Q` units (per sub-window slice, carrying frozen `bandOffsetQ`), and period-driven units keyed `(subscription, priceId, chargeKind, lineKey, AnchorPeriod)` (§4.1). |
| Period tick (T-D-15, **fact-driven since T-D-33**) | `PeriodTick` fires on **consumption of the subscriptions period-fact set** for `(subscription, billing period)` — never its own calendar — and synthesizes the period-driven units; **idempotent per `(subscription, AnchorPeriod)`** with `AnchorPeriod` ≡ the fact's period identity; a zero-usage period still emits its capacity/recurring units, now by construction (§4.2). |
| Frozen-context assembly + pin discipline (core slice [`11`](./11-consumer-contracts.md) §4.2) | `ContextAssembler` pins one committed `CatalogVersion` (published + warm-completion marker; pin lag ≤ 5s; no draft read) and freezes `Q`/`qVersion`, balances/`balanceVersion`, `fxTableVersion`, coupon snapshot, `periodState`; a missing required input fails closed here (§4.3). |
| Cascade routing (T-D-10, T-D-12) | `CascadeRouter` turns Contracts balance-effect triggers and `bandOffsetQ` shifts into delta-only `reresolve` calls, **coalesced per unit per generation** and drained on a bounded correction lane (§4.4). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-throughput-latency` | Context assembly | Assembly is frozen-input gathering + a pin; no evaluation cost lives here; the hot path is the core call over the sealed tuple | Load test (slice [`16`](./16-billing-handoff-operations.md)) |
| `cpt-cf-bss-rating-nfr-horizontal-scale` | Frozen balances (§4.5) | Because balances are frozen into the context, the core stays partition-local with **zero cross-partition locks even for pooled/committed usage**; cross-unit `balanceVersion` ordering is asynchronous on the write-back (slice 15), not a hot-path lock | Design + load test |
| `cpt-cf-bss-rating-nfr-resilience` | Period-tick idempotency + cascade coalescing | The tick is idempotent per `(subscription, AnchorPeriod)`; cascade re-resolutions coalesce per unit per generation so a fan-out is bounded and drains under backpressure without blocking first rating | Chaos/retry test |

#### Key Decisions

| Decision | Summary |
|----------|---------|
| Freeze balances, don't lock them | C1 resolution: commitment-pool balances enter the context **frozen** at a `balanceVersion`; the core never waits on a live balance; cross-unit sequencing is Contracts' serializer + the write-back (slice 15), off the hot path (§4.5). |
| Coalesce + bound the cascade | C2 resolution: cascade triggers coalesce per `(unit, generation)` and drain on a dedicated bounded lane; fan-out is finite (structural termination — core slice [`08`](./08-retroactivity-corrections.md) §4.4) and never amplifies onto the hot path (§4.4). |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-syn`

```text
Operational stores + upstream SoRs   Q store (slice 13) · pinned catalog (pricing) · Subscriptions ·
        │                            Finance FX · Promotions coupons · Billing periodState · Contracts balances
        ▼
Unit synthesis (this slice)          UnitSynthesizer · PeriodTick · ContextAssembler · CascadeRouter
        │  (freeze + invoke)
        ▼
rating-core                          evaluate() / reresolve() over the sealed EvaluationContext
        ▼
Rated output (slice 15)              outcomes + obligations + balance effects
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | Unit synthesis, the period tick, context assembly + pin, cascade routing | Rust modules in the `rating` gear (pipeline crate) |
| Domain | Unit-kind shapes, `AnchorPeriod`, context-assembly + pin descriptors, cascade generation/coalescing keys | Rust; GTS + Rust domain structs |
| Infrastructure | The period-tick coordination + idempotency ledger; the cascade lane (queue) | `toolkit-db`; coordination lease library; durable lane |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Nothing reaches the core unfrozen

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-freeze-then-invoke-syn`

Every core invocation is over a context this slice **sealed** — one `CatalogVersion`, one
`qVersion`, one `balanceVersion`, one `fxTableVersion`, one coupon snapshot, one `periodState`. A
missing input fails closed here; the core is never asked to guess or wait (core slice
[`01`](./01-foundation.md) §2.1, slice [`11`](./11-consumer-contracts.md) §2.1).

#### The tick is idempotent

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-idempotent-tick-syn`

A period tick for `(subscription, AnchorPeriod)` produces the same period-driven units however many
times it fires; a re-run (restart, retry, replay) never double-emits a recurring or capacity line
(§4.2). Idempotency is keyed, not timing-dependent.

#### Cascades drain off the hot path

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-cascade-off-hotpath-syn`

Re-resolution triggered by a correction re-enters through the core `reresolve` on a **bounded
correction lane**, coalesced per unit; it never contends with or amplifies onto first rating
(§4.4). First rating stays partition-local and cheap.

### 2.2 Constraints

#### Read-model pin discipline

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-pin-discipline-syn`

Exactly one committed `CatalogVersion` per resolution run, pin-eligible only after
`CatalogVersionPublished`, the warm-completion marker, **and every earlier version being itself
pin-eligible — the prefix-closed frontier (pricing D-114, adopted as T-D-31; this constraint is
implemented gate-side here, so the stale two-condition rule mattered)** (pin lag ≤ 5s); no draft
read, no default substitution (core slice [`11`](./11-consumer-contracts.md) §4.2; pricing design
01 §4.4).

#### Synthesis, not aggregation or evaluation

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-synthesis-only-syn`

This slice aggregates no usage (slice [`13`](./13-q-store-attribution.md) owns `Q`) and prices
nothing (the core does); it selects *what* to evaluate and freezes *the inputs*, then invokes the
core.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-syn`

- **`EvaluationUnitSpec`** — the synthesized unit + its kind (`per_event` / windowed-slice / period-driven) + the identity that keys it (usage key, `(window, slice)`, or `(subscription, priceId, chargeKind, lineKey, AnchorPeriod)`).
- **`AnchorPeriod`** — the `[periodStart, periodEnd)` UTC boundaries from the frozen `billingAnchorPolicy` (D-20 clamp); reused from core slice [`09`](./09-period-plan-change.md) `AnchorCalendar`. **Identity is consumed, not derived (T-D-33)**: for period-driven synthesis `AnchorPeriod` ≡ the consumed subscriptions fact's anchor-derived billing-period identity; the calendar supplies **geometry** (usage window slicing, the watchdog clock), and the joint K5 anchor fixture asserts calendar geometry ≡ fact identity.
- **`FrozenContext`** — the sealed `EvaluationContext` (core slice [`01`](./01-foundation.md) §3.1) with every version/pin bound: `CatalogVersion`, `(Q, qVersion)`, `(balance, balanceVersion)`, `fxTableVersion`, coupon snapshot, `periodState`.
- **`CascadeTrigger`** — a balance-effect trigger (T-D-10, from Contracts via slice [`15`](./15-rated-output-balance-effects.md)) or a `bandOffsetQ` shift (T-D-12, from slice [`13`](./13-q-store-attribution.md)); carries the affected units + the generation stamp.
- **`TickLedgerEntry`** — the idempotency record for `(subscription, AnchorPeriod)`: the tick fired, which period-driven units it emitted (§4.2).

### 3.2 Component Model

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-unit-synthesis`

- **`UnitSynthesizer`** — builds `EvaluationUnitSpec`s from usage arrival (slice 12/13), window close, the period tick, and cascade triggers.
- **`PeriodTick`** — the coordinated period-driven synthesizer, **fact-driven since T-D-33**: triggered by the subscriptions period-fact set, never a boundary clock; emits period-driven units idempotently per `(subscription, AnchorPeriod)` (§4.2).
- **`ContextAssembler`** — pins the `CatalogVersion` and freezes every other input into a `FrozenContext`; fails closed on any absent required input (§4.3).
- **`CascadeRouter`** — coalesces `CascadeTrigger`s per `(unit, generation)` and drains them as delta-only `reresolve` calls on the bounded correction lane (§4.4).

### 3.3 API Contracts

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-interface-synthesis-syn`

**Inbound**: `UsageRecord`/`Q` availability (slices [`12`](./12-usage-ingestion-normalization.md)/[`13`](./13-q-store-attribution.md)); `ReMaterializationSignal` (slice 13); balance-effect cascade triggers (slice [`15`](./15-rated-output-balance-effects.md) / Contracts); the five upstream frozen inputs (slice [`11`](./11-consumer-contracts.md) §4.2–§4.6). **Outbound**: `evaluate(FrozenContext)` / `reresolve(...)` calls into `rating-core` (in-process); the resolved outcomes flow to slice [`15`](./15-rated-output-balance-effects.md). No external synchronous API.

### 3.4 Internal Dependencies

Upstream: slices [`12`](./12-usage-ingestion-normalization.md)/[`13`](./13-q-store-attribution.md)
(usage/`Q`), slice [`11`](./11-consumer-contracts.md) (the five input contracts). Core: invokes
[`01`](./01-foundation.md) `evaluate`/`reresolve`; reuses [`09`](./09-period-plan-change.md)
`AnchorCalendar`; routes [`08`](./08-retroactivity-corrections.md) cascades. Downstream: slice
[`15`](./15-rated-output-balance-effects.md) (persists outcomes, publishes balance effects that
feed back as cascade triggers).

### 3.5 External Dependencies

| Dependency | What arrives frozen | Contract |
|------------|--------------------|----------|
| Pricing (Product Catalog) | pinned read model + `CatalogVersionPublished` + warm-completion marker | slice [`11`](./11-consumer-contracts.md) §4.2 |
| Subscriptions | phase, eligibility, seat count, `(changeEffectiveAt, changeMode)`, `(currency, region)` binding | slice [`11`](./11-consumer-contracts.md) §4.3 |
| Finance / Promotions / Billing | `fxTableVersion` / coupon snapshot / `periodState` | slice [`11`](./11-consumer-contracts.md) §4.4–§4.6 |
| Contracts | commitment-pool `(balance, balanceVersion)` frozen into the context; balance-effect cascade triggers | slice [`05`](./05-commitments-reservations.md) §4.1 (T-D-10) |

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-synthesize-syn`

**Synthesize and evaluate a usage unit**:

1. Usage/`Q` becomes available (slices [`12`](./12-usage-ingestion-normalization.md)/[`13`](./13-q-store-attribution.md)); `UnitSynthesizer` builds the `EvaluationUnitSpec` (per-event or windowed-slice, carrying `bandOffsetQ`).
2. `ContextAssembler` pins the `CatalogVersion` and freezes `(Q, qVersion)`, `(balance, balanceVersion)`, `fxTableVersion`, coupon snapshot, `periodState`; a missing required input fails closed (§4.3).
3. `rating-core` `evaluate(FrozenContext)` runs steps 1–9; the outcome flows to slice [`15`](./15-rated-output-balance-effects.md).

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-period-tick-syn`

**Period tick**:

1. The subscriptions **period-fact set** for `(subscription, billing period)` arrives — `BillableItemCreated(kind=recurring)`, one fact per component interval, on the shared ordering key — and the coordinated `PeriodTick` fires for that subscription-period (T-D-33; the calendar boundary itself triggers nothing — it only arms the missing-fact watchdog, §4.2).
2. It checks the `TickLedgerEntry` for `(subscription, AnchorPeriod)` — `AnchorPeriod` ≡ the facts' period identity; if already emitted, it is a no-op (idempotent; late-arriving facts of the same period's set extend the entry per component key, never re-emit).
3. Else the tick **records its pin first**: the run's `CatalogVersion` is written to the `TickLedgerEntry` as an **intent** before any unit evaluates — a crashed re-run reuses the recorded pin rather than re-pinning under a newer eligible version, so one period line never yields two rated rows under two pins (2026-07-31 review, #47; defence in depth — the slice-16 usage/period-key idempotency stays the primary absorber).
4. `UnitSynthesizer` synthesizes the period-driven units — **each recurring unit from its fact, inheriting the fact's `(subscriptionId, billing period, lineKey)` key**; capacity-flavor charges and true-up surfacing ride the period's plan-line fact as their trigger (T-D-33) — even for a zero-usage period (by construction: the facts arrive regardless of usage); `ContextAssembler` freezes their contexts (under the recorded pin, absorbing the fact's suspended intervals + suspension-billing posture into the recurring proration inputs and passing the period-start `payerTenantId` + `collectionPaused` marker through to the priced line), and the core evaluates them; the ledger entry commits with the emitted-unit set.

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-cascade-route-syn`

**Cascade routing**:

1. A `CascadeTrigger` arrives — a Contracts balance effect (T-D-10) or a `bandOffsetQ` shift (T-D-12) — with the affected units + a generation stamp.
2. `CascadeRouter` **coalesces** triggers for the same `(unit, generation)` into one pending re-resolution and enqueues on the bounded correction lane.
3. The lane drains: each unit `reresolve`s over its **own** pin, delta-only (core slice [`08`](./08-retroactivity-corrections.md)); the resulting deltas flow to slice [`15`](./15-rated-output-balance-effects.md). Backpressure throttles the lane; first rating is untouched (§4.4).

### 3.7 Database Schemas and Tables

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-storage-synthesis-syn`

**Owned (partitioned by the pinned `orderingTenantId`, UTC):**

- `period_tick_ledger` — PK `(subscription, AnchorPeriod)`; the idempotency record + emitted period-driven-unit set (§4.2).
- `cascade_lane` — the durable bounded queue of coalesced re-resolution work: `(unit, generation)` unique so a duplicate trigger folds in; status/backpressure columns (§4.4).

`FrozenContext` itself is not persisted authoritatively — it is assembled per run from the upstream
stores; the sealed `pricingSnapshotRef` persists with the outcome (slice
[`15`](./15-rated-output-balance-effects.md)). No monetary column.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-syn`

Pipeline crate in the one `rating` gear deployable. The `PeriodTick` runs as a **coordinated
singleton per `orderingTenantId` shard** (coordination lease library), sub-sharded by hash of
`subscriptionId` for a large tenant — the same sharding rule as slices
[`12`](./12-usage-ingestion-normalization.md)/[`13`](./13-q-store-attribution.md); the anchor-boundary
sweep for a 100K+/tenant is not serialized through one worker. The `cascade_lane` is a bounded
work queue drained by lane workers with backpressure, distinct from the first-rating path.

## 4. Additional Context

### 4.1 The Three Evaluation-Unit Kinds (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-unit-kinds-syn`

- **`per_event`** — one normalized `UsageRecord` (discrete meters); the unit is the event.
- **Windowed-`Q` (per sub-window slice)** — the aggregated `Q` for `(subscription, meter, dimensionKey, window)`; when a period split partitions the window, **one unit per sub-window slice**, each carrying the frozen `bandOffsetQ` from slice [`13`](./13-q-store-attribution.md) (T-D-12) and binding exactly one pin (core slice [`01`](./01-foundation.md) §4.2).
- **Period-driven** — recurring lines, capacity-flavor charges, and period-end true-up surfacing, keyed `(subscription, priceId, chargeKind, lineKey, AnchorPeriod)` (T-D-15; `lineKey` value rule per T-D-34, `AnchorPeriod` ≡ the consumed fact's period identity per T-D-33); synthesized by the **fact-driven** period tick, priced by the core. A **zero-usage period still emits** its period-driven units (§4.2; PRD §12 AC 20 capacity charge). This enumeration is **exhaustive**: `one_time` / `one_time_setup` rows synthesize **nothing** here — they are billed at their qualifying instant by Subscriptions/Billing from the frozen snapshot amount (T-D-18; the tick never emits, and can never re-emit, a one-time line).
- **Serialization across kinds (fills core slice [`01`](./01-foundation.md) §4.2)**: a usage unit serializes on its counter partition key `(subscription, meter, dimensionKey, window)`; a period-driven unit serializes on `(subscription, AnchorPeriod)` (the tick-ledger key). The two key spaces are disjoint by construction — a usage unit prices metered `Q`, a period-driven unit prices a recurring/capacity/true-up line — so they never contend for the same row; where a period-driven true-up reads a window aggregate, it reads it **frozen** (the window's `qVersion` at tick time), not the live counter.

### 4.2 The Period Tick (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-period-tick-syn`

- **The trigger is the subscriptions period-fact set, never the calendar (T-D-33, 2026-08-01 — resolves SEAMS SB1):** the tick synthesizes the period-driven units for `(subscription, billing period)` **on consumption of `BillableItemCreated(kind=recurring)`** — one money-free fact per component interval, cut by subscriptions' `RecurringEmitter` (subscriptions SUB-D-07/19/21, all confirmed 2026-08-01). Each recurring unit **inherits its fact's `(subscriptionId, billing period, lineKey)` key**; capacity-flavor charges and true-up surfacing ride the period's **plan-line fact** as their trigger. Still **no `rating-core` scheduler**; the tick lives here in the pipeline.
- **Absorbed fact fields (T-D-33):** the suspended interval(s) + `pause_recurring | continue` posture drive the recurring line's suspension proration (`pause_recurring` ⇒ prorate over the un-suspended stretch; `continue` ⇒ full period); the **period-start `payerTenantId`** and the `collectionPaused` marker pass through to the priced line as posting axes — no rating math reads them.
- **Missing vs late facts:** a period whose fact set has not arrived by the calendar boundary + a grace horizon raises the §17.1 charge-coverage alarm — the tick **never self-synthesizes** a period; a **late** fact set (a subscriptions §4.3b revival cut, up to the 90-day dwell) synthesizes late — its composition/payer/eligibility as-of correctness is the producer's (subscriptions SUB-D-20), and this gear prices from the frozen snapshot the facts carry.
- **Idempotent per `(subscription, AnchorPeriod)`**: the `period_tick_ledger` records that the tick fired and which units it emitted; a re-fire (restart, retry, coordinated-lease hand-off) is a no-op. A late usage arrival into an already-ticked period does **not** re-fire the tick — it re-resolves the affected usage unit (slice [`08`](./08-retroactivity-corrections.md)); a period-driven true-up whose input aggregate changed re-resolves under its own key, it is not re-synthesized.
- **Zero-usage period**: capacity-flavor charges and recurring lines still emit — **by construction since T-D-33**: the fact set arrives per component per period regardless of usage (subscriptions AC 5/27; a reservation bills its allocation regardless of usage — core slice [`05`](./05-commitments-reservations.md) §4.2; PRD §12 AC 20).
- The `AnchorCalendar` (frozen `billingAnchorPolicy`, D-20 clamp) survives as **geometry and watchdog**: the Q store keeps using it for window identity (slice [`13`](./13-q-store-attribution.md) §4.1), so period boundaries and tier-window boundaries never drift, and the **joint K5 anchor fixture asserts calendar geometry ≡ the consumed facts' period identity** — a plan change altering `billingAnchorPolicy` takes effect from the next period boundary (slice [`09`](./09-period-plan-change.md) §4.3).
- **RESOLVED — recurring period-cut ownership (SEAMS SB1 → T-D-33, 2026-08-01):** subscriptions' `RecurringEmitter` owns the recurring WHEN via its money-free period fact per `(subscriptionId, billing period, lineKey)`; this slice's tick consumes it, `AnchorPeriod` ≡ the fact's period identity (the last differing coordinate closed), and the `lineKey` value rule is adopted verbatim (T-D-34). Double-emission and missed-pause/suspension risks are gone structurally: one WHEN owner, one key — see [`../SEAMS.md`](../SEAMS.md) §K.

### 4.3 Frozen-Context Assembly and Pin Discipline (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-context-assembly-syn`

- `ContextAssembler` seals one `FrozenContext` per unit: **one committed `CatalogVersion`** (pin-eligible per the prefix-closed frontier — §2.2, T-D-31; pin lag ≤ 5s; no draft read — core slice [`11`](./11-consumer-contracts.md) §4.2), plus the frozen `(Q, qVersion)` (slice [`13`](./13-q-store-attribution.md)), `(balance, balanceVersion)` (Contracts, §4.5), **the `per_unit` quantity as a sealed pair `(N, source ref)`** — the seat count / manual quantity with its Subscriptions-side provenance, so a T-D-21 administrative re-rate or a true-up recompute of a per-seat line has a defined replay source for the historical `N` (2026-07-31 review, #18: the sealed-input enumeration had omitted it while the docs already called the value frozen in context), **the payer's `customerGroup`** (resolved from the pinned read model's membership subject — core slice [`11`](./11-consumer-contracts.md) §4.2, review #40: a period-tick unit has no caller claims to read), `fxTableVersion` (Finance), coupon snapshot (Promotions), and `(periodState, sequence)` (Billing — the T-D-28 fence coordinate).
- A missing or torn required input — no eligible pin, absent `periodState`, a torn snapshot pre-stamp/binding, an unresolvable coupon policy — **fails closed at this boundary** (core slice [`01`](./01-foundation.md) §3.3, slice [`11`](./11-consumer-contracts.md) §2.1); the core is never entered with a partial context.
- The assembled versions are exactly the determinism tuple `(window-aggregated inputs incl. bandOffsetQ, pricingSnapshotRef, fxTableVersion)` (core slice [`01`](./01-foundation.md) §4.2); re-resolution re-assembles the **same** pins (open-period) or the superseding pin (administrative re-rate — core slice [`08`](./08-retroactivity-corrections.md) §4.1).

### 4.4 Cascade Routing, Coalescing, and Bounding (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-cascade-routing-syn`

- Two frozen inputs couple units, so correcting one re-resolves its dependents (core slice [`08`](./08-retroactivity-corrections.md) §4.4): **(a)** a balance-affecting correction (T-D-10) — Contracts, the `balanceVersion` serializer, emits triggers for every later-`balanceVersion` unit that drew or was rated overage against the pool; **(b)** a `bandOffsetQ` shift (T-D-12) — slice [`13`](./13-q-store-attribution.md) emits triggers for the later slices of the window.
- **Coalescing**: `CascadeRouter` folds all triggers for the same `(unit, generation)` into **one** pending re-resolution (`cascade_lane` uniqueness) — a unit re-resolves **at most once per generation**, so a storm of triggers for the same unit does not multiply work. **The `generation` is defined (2026-07-31 review, #34 — it was load-bearing and minted nowhere)**: a **per-unit monotonic integer**, scoped to the unit key, incremented each time a *superseding* trigger source enqueues work for that unit — a newer `balanceVersion` (lane a), a newer `qVersion`/offset shift (lane b), or an administrative re-rate run (slice [`08`](./08-retroactivity-corrections.md) §4.1) — with coalescing keeping the **highest** generation (a superseded pending re-resolution is folded away, never run stale, and a correction is never dropped: the highest generation's inputs subsume its predecessors', both lanes being strictly ordered).
- **Bounding + termination**: each chain is finite and strictly ordered (balance versions / slice index — core slice [`08`](./08-retroactivity-corrections.md) §4.4), so a cascade terminates; the lane is **bounded** with backpressure, so a large fan-out drains at a controlled rate and never amplifies onto the first-rating hot path. Fan-out magnitude is bounded by the number of distinct affected units, each processed once per generation.
- Every cascade `reresolve` is delta-only under its own correction key `(unitKey[, slice], prior-rated-version, snapshot)` and its own pin; the deltas flow to slice [`15`](./15-rated-output-balance-effects.md), which dedups them (T-D-11).

### 4.5 Commitment-Balance Freezing and the Hot Path (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-balance-freeze-syn`

- Commitment-pool balances enter the `FrozenContext` **frozen at a `balanceVersion`** (the `commitmentReservation` segment — core slice [`01`](./01-foundation.md) §4.3, T-D-09/T-D-10). The core evaluates the step-6 waterfall over the frozen balance and **never reads or waits on a live balance** — so the hot path stays partition-local with **zero cross-partition locks, even for pooled/committed usage** (this is the C1 resolution: the scale NFR holds because the only cross-unit dependency is on the *write-back*, not the *read*).
- Cross-unit sequencing is asynchronous and off the hot path: slice [`15`](./15-rated-output-balance-effects.md) publishes each outcome's `CommitmentBalanceEffect` to Contracts (idempotent on the outcome key); **Contracts serializes** per-pool `balanceVersion` and freezes `(balance, balanceVersion)` for the next context assembly; concurrent units sharing a pool at one frozen version can jointly over-draw it — **the steady state under load, detected by Contracts at write-back and cascaded like any balance-affecting correction (T-D-27; core slice [`05`](./05-commitments-reservations.md) §4.1)**. A balance-affecting correction then cascades (§4.4) — bounded and coalesced.
- Consequence: pooled/committed usage has a **higher correction-cascade cost** than pure usage (a late correction can re-resolve later drawing units) but the **same first-rating cost and scalability** (frozen balance, no lock). The trade — cheap deterministic first rating, bounded asynchronous cascade — is the intended posture.

## 5. Traceability

- **PRD**: §9.2 (context inputs, Rating handoff duties), §12 AC 20 (zero-usage capacity charge), §17.1 (determinism), §6.10 (re-resolution), §7.1 (scale NFR).
- **Seams**: S1 (segments frozen into the context), M7 (`Q` frozen per `qVersion`) — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-10 (balance cascade + `balanceVersion`), T-D-12 (`bandOffsetQ` cascade), T-D-15 (period tick), T-D-16 (consolidation) — [`../DECISIONS.md`](../DECISIONS.md).
- **ADR**: [`../ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md`](../ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md).
- **Related slices**: [`01-foundation.md`](./01-foundation.md) §4.2 (unit kinds, determinism tuple), [`11-consumer-contracts.md`](./11-consumer-contracts.md) (input contracts + pin discipline), [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) (cascades, `reresolve`), [`09-period-plan-change.md`](./09-period-plan-change.md) (`AnchorCalendar`), [`13-q-store-attribution.md`](./13-q-store-attribution.md) (`Q`/`bandOffsetQ` + re-materialization signals), [`15-rated-output-balance-effects.md`](./15-rated-output-balance-effects.md) (balance-effect publication feeding cascades).
