Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Billing Handoff & Operations (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: 15-rated-output-balance-effects | Downstream: Billing | Owners: BSS Rating team -->

# DESIGN — Billing Handoff & Operations (Slice 16, pipeline)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-design-billing-handoff`

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
  - [4.1 Billing Delivery Contract (normative)](#41-billing-delivery-contract-normative)
  - [4.2 periodState Relay (normative)](#42-periodstate-relay-normative)
  - [4.3 Operational Topology and Lanes (normative)](#43-operational-topology-and-lanes-normative)
  - [4.4 Backpressure, Replay, and Cold Start (normative)](#44-backpressure-replay-and-cold-start-normative)
  - [4.5 NFR Verification (normative)](#45-nfr-verification-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

The **outbound edge** of the gear: it delivers rated charges, `Adjustment`s, and the obligations
envelope (`TrueUpObligation`, `PeriodFloorCapObligation`) to **Billing** at full intermediate
precision with the rounding-policy id — Billing aggregates the period, executes floor/cap, and
applies all rounding. It relays the inbound `periodState` (`open` / `closed_posted`) into context
assembly (slice [`14`](./14-unit-synthesis-period-tick.md)), and it owns the **operational topology**
of the whole pipeline deployable: partitioning on the M7 key, the hot / correction / balance-effect
**lanes**, backpressure, cold-start of the non-authoritative caches, and the **NFR verification home**
(p95 latency, ≥ 10M events/day/region — PRD §7.1). Consolidation (ADR-0002) makes the previously
undocumented Rating → Billing contract a first-class gear boundary here.

The two invariants it carries outward are the ones the core established and the pipeline preserved:
**full precision out, execution downstream** (the gear never rounds and never applies period-level
min/max — Billing does) and **idempotent delivery** (Billing consumes on the usage/correction key, so
a redelivery never double-posts). What crosses this edge is the money-shaped record; what stays is
every operational concern of running the gear at scale ([`../PRD.md`](../PRD.md) §6.11, §9.2, §7.1).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-contract-billing-periodstate` (core slice [`11`](./11-consumer-contracts.md) §4.6) | `BillingDelivery` emits `BillableItem`s (RatedCharge/Adjustment) + obligations at full precision + rounding-policy id; `PeriodStateRelay` feeds the inbound `periodState` into slice [`14`](./14-unit-synthesis-period-tick.md); a missing `periodState` fails closed upstream (§4.1, §4.2). |
| Floor/cap execution split (PRD §6.11) | The gear surfaces `PeriodFloorCapObligation`; **Billing executes** `max/min` at period aggregation and rounds; the non-negative guard already ran per line (core slice [`01`](./01-foundation.md) §4.4) — a floor never masks a negative line (§4.1). |
| Delta-only posted corrections (PRD §6.10) | Correction traffic reaches Billing **only** as `Adjustment`s; posted invoice lines are immutable; delivery is idempotent on the correction key (§4.1). |
| NFR show-stoppers (PRD §7.1) | This slice is the verification home: p95 lookup ≤ 100ms / path < 1s, ≥ 10M ev/day/region, deterministic replay — exercised against the pipeline end-to-end (§4.5). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-throughput-latency` | Lanes + partitioning | The hot lane (first rating) is partition-local and pure; correction and balance-effect lanes are bounded and off the hot path; delivery is an async outbox | **NFR workshop targets provisional** ([`../PRD.md`](../PRD.md) §7.1); load test here |
| `cpt-cf-bss-rating-nfr-horizontal-scale` | Partition contract | Partition by the pinned `orderingTenantId`, sub-sharded by hash of `subscriptionId`; per-counter M7 key ordering; zero cross-partition locks on the hot path | Load test at ≥ 10M ev/day/region |
| `cpt-cf-bss-rating-nfr-resilience` | Idempotent delivery + replay | At-least-once outbox, idempotent Billing consumption on the usage/correction key; deterministic replay from the authoritative stores + pinned snapshots | Chaos/retry + replay test |

#### Key Decisions

| Decision | Summary |
|----------|---------|
| Outbox delivery to Billing | The Rating → Billing handoff is a transactional-outbox event stream (at-least-once, idempotency-keyed, ordered per the M7 partition), mirroring the pricing/subscriptions outbox pattern — not a synchronous call. |
| Three lanes, one deployable | Hot (first rating), correction (cascade `reresolve`), and balance-effect publication run as distinct bounded lanes in the one `rating` gear deployable so corrections never starve or block first rating. |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-bhf`

```text
Rated output (slice 15)          RatedCharge / Adjustment + obligations + sealed ref
        ▼
Billing handoff (this slice)     BillingDelivery (outbox) · PeriodStateRelay · LaneManager ·
        │                        BackpressureController · ColdStartLoader
        ▼
Billing (external)               aggregates the period · executes floor/cap · rounds · posts
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | Billing delivery, `periodState` relay, lane/backpressure management, cold start, NFR harness | Rust modules in the `rating` gear (pipeline crate) |
| Domain | `BillableItem` delivery envelope, `periodState`, lane descriptors | Rust; GTS + Rust domain structs |
| Infrastructure | The Billing **delivery outbox**; the non-authoritative caches' cold-start loader; the durable lanes | PostgreSQL, SecureORM (`toolkit-db`); durable transport; coordination lease library |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Full precision out, execution downstream

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-precision-out-bhf`

Amounts cross to Billing at full intermediate precision with the rounding-policy id; the gear never
rounds and never applies period-level min/max — Billing aggregates, executes floor/cap, and rounds
(core slices [`01`](./01-foundation.md) §4.4 / [`09`](./09-period-plan-change.md) §2.1).

#### Idempotent delivery

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-idempotent-delivery-bhf`

Delivery is at-least-once and idempotent on the usage/correction key; a redelivery never double-posts
(the dedup guarantee of slice [`15`](./15-rated-output-balance-effects.md) extended to the wire —
Billing consumes idempotently as defense in depth).

#### Corrections never block first rating

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-lanes-bhf`

First rating, cascade re-resolution, and balance-effect publication run on **distinct bounded lanes**;
a correction storm drains under backpressure without starving or blocking the hot path (§4.3).

### 2.2 Constraints

#### The gear rounds nothing, executes no floor/cap

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-no-round-bhf`

rating-core-era invariant preserved (core slice [`09`](./09-period-plan-change.md) §2.1): obligations are
surfaced, Billing executes; the emission records the rounding-policy id, and the non-negative guard
has already run per line.

#### periodState is required, sourced only from Billing

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-periodstate-billing-bhf`

`periodState` is Billing's alone; the relay carries it into context assembly, and a missing value
fails closed upstream (core slice [`08`](./08-retroactivity-corrections.md) §4.3, slice
[`14`](./14-unit-synthesis-period-tick.md) §4.3) — never guessed here.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-bhf`

- **`BillableItemDelivery`** — the outbound envelope: `RatedCharge` / `Adjustment` + obligations + sealed `pricingSnapshotRef` + `{skuId, planId, priceId}` + rounding-policy id + **`glCode`** (rides the `RatedCharge`, slice [`15`](./15-rated-output-balance-effects.md) §3.1 — Billing posts to GL from it, no catalog re-query) + the delivery idempotency key (usage/correction key).
- **`PeriodStateInput`** — the inbound `periodState ∈ {open, closed_posted}` for a subscription/period, relayed to slice [`14`](./14-unit-synthesis-period-tick.md).
- **`Lane`** — a bounded work lane descriptor (hot / correction / balance-effect) with its concurrency + backpressure policy.
- **`DeliveryOutboxEntry`** — the transactional outbox row for one Billing delivery; at-least-once, idempotency-keyed.

### 3.2 Component Model

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-billing-handoff`

- **`BillingDelivery`** — writes `BillableItemDelivery` to the transactional outbox (same commit as rated output) and relays to Billing at-least-once, ordered per M7 partition, idempotency-keyed.
- **`PeriodStateRelay`** — ingests Billing's `periodState` and makes it available to context assembly (slice [`14`](./14-unit-synthesis-period-tick.md)); fail-closed absence.
- **`LaneManager`** — runs the hot / correction / balance-effect lanes with distinct bounded concurrency (§4.3).
- **`BackpressureController`** — throttles the correction/balance-effect lanes under load so the hot lane keeps its latency budget (§4.4).
- **`ColdStartLoader`** — warms the non-authoritative caches (resolved-window cache — core slice [`01`](./01-foundation.md) §3.7; FX-table pages) after a restart; correctness never depends on a warm cache (§4.4).

### 3.3 API Contracts

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-interface-billing-handoff-bhf`

**Outbound (to Billing)**: the `BillableItemDelivery` event stream — transport, ordering (per M7
partition), and idempotent consumption on the usage/correction key; full-precision amounts +
rounding-policy id + obligations. **Inbound (from Billing)**: `periodState`. The Billing-facing
contract shape is co-owned with Billing (core slice [`11`](./11-consumer-contracts.md) §4.6); this
slice fixes the Rating-side delivery semantics + the operational transport.

### 3.4 Internal Dependencies

Upstream: slice [`15`](./15-rated-output-balance-effects.md) (the rated output + obligations
delivered). Feeds context assembly (slice [`14`](./14-unit-synthesis-period-tick.md)) with
`periodState`. Preserves the obligation/precision invariants of core slices
[`01`](./01-foundation.md)/[`09`](./09-period-plan-change.md). Boundary contract: slice
[`11`](./11-consumer-contracts.md) §4.6.

### 3.5 External Dependencies

| Dependency | What crosses the boundary | Contract |
|------------|---------------------------|----------|
| Billing | `BillableItem`/`Adjustment` + obligations out (full precision); `periodState` in; Billing aggregates, executes floor/cap, rounds, posts | core slice [`11`](./11-consumer-contracts.md) §4.6; PRD §6.11, §9.2 |

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-deliver-bhf`

**Deliver to Billing**:

1. Slice [`15`](./15-rated-output-balance-effects.md) commits a `RatedCharge`/`Adjustment` + obligations; `BillingDelivery` writes the `DeliveryOutboxEntry` in the **same commit** (no rated output without its delivery queued, no delivery without a committed outcome).
2. The outbox relays to Billing at-least-once, ordered per the M7 partition, keyed by the usage/correction key.
3. Billing consumes idempotently, aggregates the period, executes `max(total, floor)`/`min(total, cap)` from the obligation, and rounds; a redelivery is a no-op on Billing's side.
4. Corrections arrive as `Adjustment`s only; posted invoice lines stay immutable.

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-flow-periodstate-relay-bhf`

**periodState relay**: Billing publishes `periodState` transitions **each carrying a
per-`(subscription, period)` sequence (T-D-28, 2026-08-01 — joint with Billing, flagged for
veto)**; `PeriodStateRelay` applies them **highest-sequence-wins** (never last-write-wins — an
older transition must not overwrite a newer one, and plain forward-only would be wrong since a
window can legitimately re-open) and makes the current `(periodState, sequence)` pair available
to slice [`14`](./14-unit-synthesis-period-tick.md) context assembly, which freezes it into the
`EvaluationContext`; correction routing re-reads the projection at route time and records the
observed pair (core slice [`08`](./08-retroactivity-corrections.md) §4.3); a missing value fails
closed at assembly (never guessed).

### 3.7 Database Schemas and Tables

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-storage-billing-handoff-bhf`

**Owned (partitioned by the pinned `orderingTenantId`, UTC):**

- `billing_delivery_outbox` — the transactional outbox for `BillableItemDelivery` (committed with slice 15's `rated_output`); at-least-once, idempotency-keyed on the usage/correction key, ordered per M7 partition.
- `period_state` — the relayed Billing `(periodState, sequence)` per subscription/period (a projection, non-authoritative — Billing is the SoR; refreshed on Billing events, **highest-sequence-wins**, T-D-28).

Concrete DDL is Design. No monetary computation, no floor/cap, no rounding here.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-bhf`

The one `rating` gear deployable, partitioned by the pinned `orderingTenantId` and sub-sharded by
hash of `subscriptionId`. **Three lanes** run per shard (coordination lease library): the **hot lane**
(ingestion → Q → synthesis → core → rated output → delivery, partition-local, latency-budgeted); the
**correction lane** (cascade `reresolve`, bounded, backpressured — slice [`14`](./14-unit-synthesis-period-tick.md) §4.4); and the **balance-effect lane** (the outbox relay to Contracts — slice
[`15`](./15-rated-output-balance-effects.md) §4.5). `rating-core` remains a pure I/O-free crate the
hot and correction lanes call (ADR-0002); the non-authoritative caches are per-instance and
cold-startable (§4.4).

## 4. Additional Context

### 4.1 Billing Delivery Contract (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-billing-delivery-bhf`

- Delivery carries `RatedCharge`/`Adjustment` + obligations (`TrueUpObligation`, `PeriodFloorCapObligation`) + the sealed `pricingSnapshotRef` + `{skuId, planId, priceId}` + the rounding-policy id + the `glCode` (on the `RatedCharge` — the journal-entry input, slice [`10`](./10-governance-asc606.md)), at **full intermediate precision** (the concrete DECIMAL precision is fixed with Billing before Design lock — core slice [`01`](./01-foundation.md) §1.2, still open).
- **Billing executes and rounds**: `max(total, floor)`/`min(total, cap)` at period aggregation, then the rounding policy; the gear applies neither (core slice [`09`](./09-period-plan-change.md) §2.1). The per-line non-negative guard has already run (core slice [`01`](./01-foundation.md) §4.4) — a floor never masks a negative line.
- **Corrections deliver only as `Adjustment`s** on the correction key; posted invoice lines are immutable (PRD §6.10). Delivery is **idempotent** on the usage/correction key: at-least-once transport + idempotent Billing consumption (defense in depth over slice [`15`](./15-rated-output-balance-effects.md)'s dedup).
- Ordering: deliveries are ordered per the M7 partition `(subscription, meter, dimensionKey, window)` and, for period-driven units, per `(subscription, AnchorPeriod)` — the same partition discipline the whole gear rides (SEAMS M7; shared ordering key with subscriptions, SUB-R1).

### 4.2 periodState Relay (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-periodstate-relay-bhf`

- `periodState ∈ {open, closed_posted}` is **Billing's alone**; the relay carries the current value into slice [`14`](./14-unit-synthesis-period-tick.md) context assembly, which freezes it into the `EvaluationContext`. `open` routes late/corrected usage through pinned-snapshot re-resolution; `closed_posted` engages posted-period delta-only protection (core slice [`08`](./08-retroactivity-corrections.md) §4.3).
- **Missing `periodState` fails closed** at assembly (core slice [`14`](./14-unit-synthesis-period-tick.md) §4.3) — never guessed here; the relay's `period_state` projection is non-authoritative (Billing is the SoR) and is refreshed on Billing events.

### 4.3 Operational Topology and Lanes (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-lanes-bhf`

- The gear runs as one deployable, partitioned by the pinned `orderingTenantId`, sub-sharded by hash of `subscriptionId` (the same rule as the subscriptions gear jobs), so no single worker funnels a large tenant's ingestion, period tick, or delivery.
- **Three bounded lanes** per shard: the **hot lane** (first rating, partition-local, pure core call, latency-budgeted); the **correction lane** (cascade `reresolve` — coalesced and bounded, slice [`14`](./14-unit-synthesis-period-tick.md) §4.4); the **balance-effect lane** (outbox relay to Contracts, slice [`15`](./15-rated-output-balance-effects.md) §4.5). A correction/cascade storm drains on its own lane under backpressure and **never blocks or starves first rating** (the C2 fan-out bound realized operationally).
- Single-writer per M7 counter key is realized by partition ownership (slice [`13`](./13-q-store-attribution.md) §3.8); zero cross-partition locks on the hot path — including for pooled/committed usage, because balances are frozen into the context (slice [`14`](./14-unit-synthesis-period-tick.md) §4.5, the C1 resolution).

### 4.4 Backpressure, Replay, and Cold Start (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-backpressure-replay-bhf`

- **Backpressure** is transport-native: an unacked ingestion offset (slice [`12`](./12-usage-ingestion-normalization.md)) or a full correction lane throttles intake rather than dropping work; a Billing-delivery outbox that cannot drain applies backpressure to its lane, never a silent drop.
- **Replay** is deterministic end-to-end: the authoritative stores (`usage_record`/`usage_dedup` s12, `windowed_counter` s13, `rated_output`/`delta_dedup` s15) plus the pinned snapshots reproduce any outcome byte-identically (core slice [`01`](./01-foundation.md) §4.2); a lost non-authoritative cache or an in-flight lane is rebuilt by replay, never by a guess.
- **Cold start**: after a restart, `ColdStartLoader` warms the resolved-window cache (core slice [`01`](./01-foundation.md) §3.7) and FX-table pages; correctness never depends on a warm cache — a cold read degrades latency only. Delivery/outbox relays resume from their durable offsets; the dedup indexes make resumed work idempotent.

### 4.5 NFR Verification (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-nfr-verification-bhf`

- This slice is the **NFR verification home** for the gear (PRD §7.1): p95 catalog-lookup ≤ 100ms and per-line path < 1s (the lookup cost is the read-model pin, not evaluation — core slice [`01`](./01-foundation.md) §1.2); throughput **≥ 10M events/day/region** on the ingestion → delivery hot lane; deterministic replay and idempotent retry across all lanes.
- **Targets are provisional pending the program NFR workshop** ([`../PRD.md`](../PRD.md) §7.1); the verification plan (load at ≥ 10M ev/day/region, chaos/retry, replay-equivalence, and a correction-storm test asserting the hot lane keeps its budget under a bounded cascade) is exercised against the assembled pipeline here.
- **Scale-posture record (from the design review)**: pure/first-rating usage is partition-local with zero cross-partition locks; pooled/committed usage keeps the same first-rating scalability (frozen balance) at a higher **bounded** correction-cascade cost (slice [`14`](./14-unit-synthesis-period-tick.md) §4.4/§4.5) — the verification MUST include a pooled-usage cascade scenario, not only pure usage.

## 5. Traceability

- **PRD**: §9.2 (Billing contract), §6.11 (floor/cap execution split), §6.10 (delta-only corrections), §7.1 (NFR show-stoppers), §4.1 (precision).
- **Seams**: M7 (partition/ordering), B1 (bundle lineage delivered) — [`../SEAMS.md`](../SEAMS.md); shared ordering key with subscriptions (SUB-R1).
- **Decisions**: T-D-16 (consolidation — the Rating → Billing contract becomes first-class here); T-D-10/T-D-11 (balance-effect + delta lanes) — [`../DECISIONS.md`](../DECISIONS.md).
- **ADR**: [`../ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md`](../ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md).
- **Related slices**: [`09-period-plan-change.md`](./09-period-plan-change.md) (obligation shapes + no-round invariant), [`11-consumer-contracts.md`](./11-consumer-contracts.md) §4.6 (Billing contract), [`15-rated-output-balance-effects.md`](./15-rated-output-balance-effects.md) (upstream store + outbox), [`14-unit-synthesis-period-tick.md`](./14-unit-synthesis-period-tick.md) (periodState consumer, lanes), [`12-usage-ingestion-normalization.md`](./12-usage-ingestion-normalization.md)/[`13-q-store-attribution.md`](./13-q-store-attribution.md) (partitioning, backpressure).
