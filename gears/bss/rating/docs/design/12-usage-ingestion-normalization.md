Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Usage Ingestion & Normalization (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: OSS Metering, Subscriptions | Downstream: 13-q-store-attribution, rating-core | Owners: BSS Rating team -->

# DESIGN — Usage Ingestion & Normalization (Slice 12, pipeline)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-design-usage-ingestion`

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
  - [4.1 Normalization to UsageRecord (normative)](#41-normalization-to-usagerecord-normative)
  - [4.2 Continuous-Duration Session Merge (normative)](#42-continuous-duration-session-merge-normative)
  - [4.3 Authoritative Usage Dedup (normative)](#43-authoritative-usage-dedup-normative)
  - [4.4 Correction Ingestion (normative)](#44-correction-ingestion-normative)
  - [4.5 Fail-Closed Intake and Quarantine (normative)](#45-fail-closed-intake-and-quarantine-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

The **intake edge** of the rating pipeline: it receives raw usage events, normalizes them into
canonical `UsageRecord`s (units, UTC timestamps, tenant axes, subscription/meter linkage,
`dimensionKey` value pass-through from OSS metering), merges contiguous usage of
continuous-duration meters into session/window measures **before** the core's granularity
round-up, and owns the **authoritative usage idempotency/dedup** — the same source event never
contributes to `Q` twice. Correcting/negative usage enters the pipeline here (ingestion + dedup are
pipeline-side; the reversal *math* is the core's — slice [`08`](./08-retroactivity-corrections.md)).

This slice is deliberately **content-free about price**: it computes no money, resolves no catalog,
and reads no snapshot — it produces the clean, deduplicated, canonical measure the windowed `Q`
store (slice [`13`](./13-q-store-attribution.md)) counts and the core prices. Everything the core's
determinism contract depends on being *frozen* starts life here as a normalized, immutable
`UsageRecord`; a raw event that cannot be normalized is **quarantined, never guessed or dropped**
([`../PRD.md`](../PRD.md) §6.1, §6.7, §9.2; ADR-0002 — this slice absorbs the ingestion half of the
VHP-810 rating-engine scope).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-fr-idempotency` (usage family) | The `UsageDedupStore` is the **authoritative** dedup: a source event keyed `(sourceSystem, meterId, sourceEventId)` yields at most one `UsageRecord` contribution; a duplicate returns the recorded record id and never re-counts into `Q` (§4.3). |
| Merge-before-round-up (PRD §17.1 step 3) | The `SessionMerger` folds contiguous/overlapping intervals of a continuous-duration meter into one session measure **before** the core's granularity round-up — the core prices the merged aggregate, never raw records (§4.2; core slice [`03`](./03-metering-models.md) §4.4). |
| `dimensionKey` value pass-through (PRD §6.7, §17.3) | The `Normalizer` copies `dimensionKey` **values** from OSS metering verbatim into the `UsageRecord`; it never fabricates, defaults, or collapses a dimension value — an undeclared/partial tuple is the core's routing concern (slice [`03`](./03-metering-models.md) §4.2), not an intake guess (§4.1). |
| Correction ingestion (PRD §6.10) | A correcting/negative event is a first-class `UsageRecord` carrying a `corrects` lineage reference; it is **not** a dedup collision — the replay/diff math is core slice [`08`](./08-retroactivity-corrections.md) (§4.4). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-throughput-latency` (≥ 10M ev/day/region) | Ingestion consumer + dedup write path | The write-heavy edge of the gear: a durable at-least-once consumer, partition-local dedup upserts on `(orderingTenantId)` shards, sub-sharded by hash of `subscriptionId`; no catalog read, no core call on the intake path | Load test (slice [`16`](./16-billing-handoff-operations.md) NFR home) |
| `cpt-cf-bss-rating-nfr-resilience` | Dedup + quarantine | At-least-once redelivery is absorbed by the idempotent dedup upsert; an un-normalizable event is quarantined (dead-letter), never silently dropped; replay of the source stream re-derives byte-identical `UsageRecord`s | Chaos/retry test |
| `cpt-cf-bss-rating-nfr-horizontal-scale` | Partition contract | Dedup and merge stay inside the `(orderingTenantId)` partition and the `(subscription, meter, …)` aggregate; no cross-partition coordination | Design + load test |

#### Key Decisions

| Decision | Summary |
|----------|---------|
| Two dedup layers, distinct owners | **Ingestion dedup** (this slice, source-event identity → one `Q` contribution) is separate from **rated-output dedup** (slice [`15`](./15-rated-output-balance-effects.md), usage key → one `RatedCharge`, correction key → one `Adjustment`, T-D-11); the ingestion usage key propagates into the rated-output key so "same key + same snapshot never double-charges" (core slice [`01`](./01-foundation.md) §4.2). |
| T-D-16 scope absorption | This slice is the ingestion half of the consolidated pipeline (ADR-0002); it owns the **first authoritative store** in a gear whose evaluation core owns none. |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-ing`

```text
Raw usage transport (external)   OSS metering usage events (at-least-once, durable) ·
        │                        Subscriptions meter↔subscription linkage (frozen ctx)
        ▼
Ingestion edge (this slice)      IntakeConsumer · Normalizer · SessionMerger · UsageDedupStore ·
        │                        CorrectionIntake · QuarantineSink
        ▼
Windowed Q store (slice 13)      normalized UsageRecords counted into Q (single-writer)
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | The intake consumer, normalization, session merge, dedup, correction intake, quarantine | Rust modules in the `rating` gear (pipeline crate; **not** `rating-core`) |
| Domain | Raw-event and `UsageRecord` shapes, session-merge geometry, dedup/correction keys | Rust; GTS + Rust domain structs |
| Infrastructure | The **usage dedup store** and the append-only `usage_record` store; the quarantine (dead-letter) store | PostgreSQL, SecureORM (`toolkit-db`); durable event transport |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Normalize once, freeze forever

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-principle-normalize-once-ing`

A raw event becomes exactly one immutable `UsageRecord`; downstream stages (Q, core, output) read
it frozen and never re-normalize. Re-ingesting the same source event replays to the same record —
normalization is a pure function of the raw event plus the frozen unit-identity catalog (§4.1).

#### Merge before the core rounds

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-principle-merge-before-round-ing`

Continuous-duration usage is merged into a session/window measure here, **before** the core's
granularity round-up — the core prices the merged aggregate, never raw samples (PRD §17.1 step 3;
core slice [`03`](./03-metering-models.md) §4.4). Merge is deterministic and idempotent (§4.2).

#### Pass dimensions through, never author them

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-principle-dimension-passthrough-ing`

`dimensionKey` values are OSS metering's (the external critical path); this slice copies them
verbatim and never fabricates, defaults, or collapses a value. Declaration is the catalog's, freeze
is the core's (slice [`03`](./03-metering-models.md) §4.2) — intake only carries (§4.1).

### 2.2 Constraints

#### Authoritative usage dedup

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-constraint-authoritative-dedup-ing`

Usage dedup is authoritative **here** (PRD §6.1 `fr-idempotency`; core slice [`01`](./01-foundation.md)
§4.2): a source event contributes to `Q` at most once. This is distinct from — and upstream of —
the rated-output dedup owned by slice [`15`](./15-rated-output-balance-effects.md).

#### No price, no snapshot, no catalog

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-constraint-no-price-ing`

Intake computes no money, pins no `pricingSnapshotRef`, and reads no catalog — it produces the
canonical measure; the core does all evaluation over the frozen inputs assembled later (slice
[`14`](./14-unit-synthesis-period-tick.md)).

#### UTC, canonical units

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-constraint-utc-units-ing`

All timestamps are UTC; measures carry the meter's canonical unit (unit identity is immutable —
GB ≠ GiB is a different unit, not a conversion; a unit correction is a new unit — aligned with the
registry's deprecate-then-remove doctrine, [`../SEAMS.md`](../SEAMS.md) §I). Intake never converts
units.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-domain-model-ing`

- **`RawUsageEvent`** — the wire input from OSS metering: source system, `sourceEventId`, meter id, subscription/resource linkage, quantity or start/stop interval, unit, `dimensionKey` values, event time (UTC), optional `corrects` reference.
- **`UsageRecord`** — the normalized, immutable output: `usageKey` (§4.3), tenant axes (`orderingTenantId`, `resourceTenantId`, `payerTenantId`, `sellerTenantId`), subscription + meter + `dimensionKey`, canonical measure, event-time window coordinates, session-merge lineage, `corrects` reference (if any), source lineage.
- **`SessionMeasure`** — for continuous-duration meters: the merged half-open `[start, stop)` interval set and its total measure; the merge lineage (which raw records folded in).
- **`UsageDedupEntry`** — `usageKey` → recorded `UsageRecord` id + digest; the authoritative dedup fact (§4.3).
- **`QuarantineEntry`** — an un-normalizable raw event + typed reason (§4.5); never silently dropped, operator-visible, replayable after remediation.

### 3.2 Component Model

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-usage-ingestion`

- **`IntakeConsumer`** — the durable at-least-once transport consumer; hands each `RawUsageEvent` to normalization; commits its transport offset only after the dedup upsert commits (no lost, no double-counted event).
- **`Normalizer`** — `RawUsageEvent` → `UsageRecord`: canonical units, UTC coordinates, tenant-axis + subscription/meter resolution against the frozen context, `dimensionKey` verbatim pass-through; a resolution failure routes to quarantine (§4.5).
- **`SessionMerger`** — folds contiguous/overlapping continuous-duration intervals into a `SessionMeasure` before round-up (§4.2).
- **`UsageDedupStore`** — the authoritative dedup upsert on `usageKey`; returns the recorded id on a duplicate (§4.3).
- **`CorrectionIntake`** — admits correcting/negative events as first-class `UsageRecord`s with a `corrects` reference; routes the replay/diff to core slice [`08`](./08-retroactivity-corrections.md) via the Q store re-materialization (slice [`13`](./13-q-store-attribution.md)) (§4.4).
- **`QuarantineSink`** — the dead-letter store for un-normalizable events (§4.5).

### 3.3 API Contracts

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-interface-ingest-ing`

**Inbound (consumed)**: the OSS metering usage transport — at-least-once, durable, ordered per
source partition; the raw-event schema is an OSS-metering contract this slice adopts. **Outbound
(provided in-process to slice [`13`](./13-q-store-attribution.md))**: `UsageRecord`s (new and
correcting) with their `usageKey`, ready to count into `Q`. No external synchronous API — intake is
transport-driven; the Subscriptions meter↔subscription linkage arrives as frozen context.

### 3.4 Internal Dependencies

Downstream: slice [`13`](./13-q-store-attribution.md) (counts `UsageRecord`s into `Q`), core slice
[`03`](./03-metering-models.md) (prices the merged aggregate), core slice
[`08`](./08-retroactivity-corrections.md) (correction replay math). Boundary/context contracts:
slice [`11`](./11-consumer-contracts.md) (Subscriptions input; the frozen linkage). No dependency on
`rating-core` internals — intake precedes evaluation.

### 3.5 External Dependencies

| Dependency | What crosses the boundary | Contract |
|------------|---------------------------|----------|
| OSS Metering (Usage Collector) | raw usage events (quantity/interval, unit, `dimensionKey` values, event time), at-least-once durable | PRD §6.7, §17.3; the external critical path. **The built v1 collector has no emission surface (pull/query only)** — the transport, attribution join, dedup-key derivation, and correction visibility are tracked as [`../SEAMS.md`](../SEAMS.md) §J (UC1–UC6; UC6 = temporal shape — v1 carries point-stamped `(value, created_at)` only, duration rides chunked counter deltas until a native interval kind lands); **UC1 gates this slice's implementation** |
| Subscriptions | meter ↔ subscription linkage, tenant axes, the pinned `orderingTenantId` | slice [`11`](./11-consumer-contracts.md) §4.3; SEAMS S1 / SUB-R1 |

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-ingest-ing`

**Ingest one usage event**:

1. `IntakeConsumer` receives a `RawUsageEvent` from the durable transport.
2. `Normalizer` resolves tenant axes + subscription/meter linkage and produces a canonical `UsageRecord` (UTC, canonical unit, `dimensionKey` verbatim); an unresolvable event routes to `QuarantineSink` (§4.5) and the offset still commits (poison message never blocks the partition).
3. `UsageDedupStore` upserts on `usageKey`: a first sighting persists the `UsageRecord`; a duplicate returns the recorded id and stops (no re-count).
4. For continuous-duration meters, `SessionMerger` folds the record into its open session measure (§4.2) — idempotently, keyed by the same `usageKey` set.
5. The transport offset commits **after** the dedup upsert; the `UsageRecord` is now visible to the Q store (slice [`13`](./13-q-store-attribution.md)).

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-flow-correction-intake-ing`

**Correction intake**: a correcting/negative event carries a `corrects` reference; it is admitted
as a new `UsageRecord` (its own `usageKey`), links to the corrected record, and signals slice
[`13`](./13-q-store-attribution.md) to re-materialize the affected window `Q` — the reversal math
runs in core slice [`08`](./08-retroactivity-corrections.md). A correction is never a dedup
collision with the record it corrects.

### 3.7 Database Schemas and Tables

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-storage-ingestion-ing`

**Owned (the gear's first authoritative stores; partitioned by the pinned `orderingTenantId`, UTC):**

- `usage_record` — append-only normalized records; indexed by `(subscription, meter, dimensionKey, event-time window)` to feed the Q store; immutable.
- `usage_dedup` — unique index on `usageKey`; the authoritative ingestion dedup fact.
- `usage_quarantine` — dead-letter store: raw event + typed reason + remediation/replay status.

Concrete DDL is Design; the append-only + immutability posture mirrors the core's frozen-input
doctrine. No monetary column (the gear computes no money — core slice [`01`](./01-foundation.md) §3.7).

#### Hot/cold tiering of `usage_record` (storage posture)

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-storage-tiering-ing`

`usage_record` is tiered; the guarantees are tier-invariant. **Hot** (PostgreSQL): open windows
plus a trailing K months past `closed_posted` (K is deployment-tuned; correction traffic decays
fast). **Cold**: an **S3-compatible object store** holding immutable, digest-verified columnar
objects (Parquet) to the full ≥ 7-year correction horizon (core slice
[`08`](./08-retroactivity-corrections.md) §4.1), partitioned by `(orderingTenantId, window)` and
sorted `(subscription, meter, eventTime)` for partition/row-group pruning. A per-shard **archiver**
job (coordination lease, same sharding line as the other pipeline jobs) rewrites hot rows into
objects, **verifies row count + content digest against the manifest, and only then prunes hot** —
never an unverified delete. The `usage_archive_manifest` table (partition → object keys, row
count, digest) is authoritative state and is backed up with the hot store; loss is repaired by
bucket listing + digest re-verification.

Cold is read **only off the hot path** — three consumers: correction-lane re-materialization
(slice [`13`](./13-q-store-attribution.md) §4.4 reads the contributing record set), dispute/audit
evidence, and DR rebuild. A fetch hydrates the partition (optionally into a bounded-TTL
rehydration cache so a cascade over the same window fetches once); immutable objects + the
recorded digest preserve the determinism contract across tiers (same record set ⇒ same `Q`).
Object-store unavailability backpressures the correction lane fail-closed; **first rating never
reads cold**. Tiering applies to `usage_record` only: `usage_dedup` stays hot forever (tiny rows,
unbounded window — archiving it would silently shrink the dedup guarantee), and
`usage_quarantine` follows its remediation lifecycle. **Pinned here**: the contract (S3-compatible
API, object immutability/lock, digest verification, the partition layout, cold-reads-only-off-hot-path).
**Deployment profile**: the concrete endpoint (e.g. VHI S3 / MinIO / cloud S3) and K.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-ing`

Pipeline crate inside the one `rating` gear deployable (ADR-0002; distinct from the `rating-core`
crate). The `IntakeConsumer` runs as a **partitioned consumer group**: one consumer per
`orderingTenantId` shard, sub-sharded by hash of `subscriptionId` for a large tenant (the same
sharding rule the subscriptions gear applies to its jobs), so the ≥ 10M ev/day/region write edge is
never funnelled through one worker; dedup upserts are partition-local. Backpressure is the durable
transport's (unacked offsets), never a silent drop.

## 4. Additional Context

### 4.1 Normalization to UsageRecord (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-normalization-ing`

- Normalization is a **pure function** of `(RawUsageEvent, frozen unit-identity + subscription/meter linkage)`: replay of the source stream re-derives byte-identical `UsageRecord`s.
- `dimensionKey` **values** are copied verbatim from OSS metering; intake never fabricates, defaults, or collapses a value (declaration = catalog, freeze = core slice [`03`](./03-metering-models.md); PRD §6.7).
- Units are canonical and immutable (GB ≠ GiB); intake performs **no** unit conversion — a wrong unit is a quarantine, not a silent coercion (§4.5).
- **Gauge samples (D-44 / T-D-17)**: for a meter whose frozen `aggregationFunction ≠ sum`, records are point-stamped **level samples** (collector `gauge` kind) in the meter's **level unit** (GB, cloudlet — validated at publish against the billable level·granule unit). Intake normalizes and dedups them exactly like counter deltas — it never folds, never integrates, never fills gaps (the granule fold is the Q store's, slice [`13`](./13-q-store-attribution.md); the `hold_last`/`maxHold` gap policy is applied at fold time, not at intake).
- Every `UsageRecord` carries the full tenant-axis set and the pinned `orderingTenantId` for partition alignment with the core's determinism key and the subscriptions ordering key (SEAMS S1 / SUB-R1).

### 4.2 Continuous-Duration Session Merge (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-session-merge-ing`

- For a continuous-duration meter, contiguous or overlapping `[start, stop)` intervals for the same `(subscription, meter, dimensionKey)` are merged into one **session measure** before the core's granularity round-up (PRD §17.1 step 3): twelve 5-minute samples become one measured span, not twelve rounded units (core slice [`03`](./03-metering-models.md) §4.4).
- Merge is **deterministic and idempotent**: overlapping intervals union (no double-count of an overlap); a re-delivered interval folds to the same session by `usageKey`; half-open UTC geometry means a boundary instant belongs to exactly one interval.
- The merge lineage (which raw records folded in) is retained so a correction to one contributing record re-materializes the session deterministically (slice [`13`](./13-q-store-attribution.md), core slice [`08`](./08-retroactivity-corrections.md)).
- Discrete / `per_event` meters are **not** merged — each event is its own measure (core slice [`01`](./01-foundation.md) §4.2 `per_event` unit).

### 4.3 Authoritative Usage Dedup (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-usage-dedup-ing`

- The **usage key** is `(sourceSystem, meterId, sourceEventId)`; where a source cannot supply a stable event id, a content digest over the canonical fields is the fallback key (recorded so the choice is auditable). **The digest is pinned (2026-07-31 review, #42 — a dedup key must be stable across releases and reproducible on replay)**: SHA-256 over the canonical serialization of exactly `(tenant_id, usageTypeRef, resource_ref, subject_ref, event time, value, unit, metadata as sorted k=v pairs)`, recorded beside the key with **`digest_key_version = 1`** — a field-set or algorithm change bumps the version, and records deduplicate only within one version. Latent at launch: the only launch source (the Usage Collector) always supplies the stable `(tenant_id, gts_id, idempotency_key)` identity (SEAMS UC4), so the branch is not exercised. Dedup is a unique-index upsert: a first sighting persists, a duplicate returns the recorded `UsageRecord` id and **does not** re-count into `Q`.
- Dedup is **authoritative here** — the single source of truth that a usage event contributes to `Q` at most once (PRD §6.1; core slice [`01`](./01-foundation.md) §4.2). This is a **distinct layer** from the rated-output dedup (slice [`15`](./15-rated-output-balance-effects.md): `RatedCharge` per usage key, `Adjustment` per correction key, T-D-11); the ingestion `usageKey` **propagates** into the rated-output key so the two layers compose ("same key + same snapshot never double-charges").
- At-least-once transport redelivery, consumer restart, and source-stream replay are all absorbed by the idempotent upsert — none double-counts.

### 4.4 Correction Ingestion (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-correction-intake-ing`

- A correcting or negative usage event is admitted as a **first-class `UsageRecord`** with its own `usageKey` and a `corrects` lineage reference; it is **never** a dedup collision with the record it adjusts.
- Correction ingestion stays pipeline-side (PRD §6.10); the record signals slice [`13`](./13-q-store-attribution.md) to **re-materialize** the affected window `Q`, and the replay/diff/reversal math runs in core slice [`08`](./08-retroactivity-corrections.md) over the pinned snapshot — intake carries the corrected measure, never computes the delta.
- Late arrival vs `periodState` (`open` / `closed_posted`) is **not** decided here: intake tags event time; routing to open-period re-resolution or posted-period protection is the core's (slice [`08`](./08-retroactivity-corrections.md) §4.3), driven by the Billing `periodState` assembled in slice [`14`](./14-unit-synthesis-period-tick.md).

### 4.5 Fail-Closed Intake and Quarantine (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-quarantine-ing`

- An event that cannot be normalized — unknown meter, unresolvable subscription linkage, non-canonical/unknown unit, malformed interval — is routed to `usage_quarantine` with a **typed reason**; it is **never silently dropped and never guessed** (the fail-closed doctrine of core slice [`01`](./01-foundation.md) §2.1).
- Quarantine is operator-visible and **replayable**: after the upstream defect is fixed (e.g. the meter is declared, the subscription linkage lands), the event replays through normalization deterministically. **Replay is an authorized operator action** — `usage × replay` under the gear authz catalog (slice [`10`](./10-governance-asc606.md) §4.6): replaying quarantined usage creates billable charges, so it is never an unauthenticated maintenance call; the actor and replayed set are audited (2026-07-31 review, #51).
- A poison event never blocks its partition: the transport offset commits after quarantine so the consumer group makes progress; correctness is preserved because a quarantined event contributes nothing to `Q` until replayed.

## 5. Traceability

**Traces to**: `cpt-cf-bss-rating-fr-dimension-population-contract`

- **PRD**: §9.2 (Rating handoff duties — usage dedup, merge), §6.1 (`fr-idempotency` usage family), §6.7 (`dimensionKey` values), §6.10 (correction ingestion), §17.1 step 3 (merge-before-round-up), §7.1 (throughput NFR).
- **Seams**: M7 (feeds the single-writer Q store), S1 / SUB-R1 (pinned `orderingTenantId`), **UC1–UC6 §J** (Usage Collector ingestion bridge — transport, watermark, attribution join, dedup-key derivation, correction visibility, temporal shape; UC1 gates implementation) — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-04 (counter key the records feed), T-D-11 (rated-output dedup is a distinct downstream layer), T-D-16 (consolidation; this slice absorbs VHP-810 ingestion scope) — [`../DECISIONS.md`](../DECISIONS.md).
- **ADR**: [`../ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md`](../ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md).
- **Related slices**: [`13-q-store-attribution.md`](./13-q-store-attribution.md) (counts the records), [`01-foundation.md`](./01-foundation.md) / [`03-metering-models.md`](./03-metering-models.md) / [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) (core consumers), [`11-consumer-contracts.md`](./11-consumer-contracts.md) (Subscriptions input).
