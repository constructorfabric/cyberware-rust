Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Rated Output, Delta Dedup & Balance Effects (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: rating-core, 14-unit-synthesis-period-tick | Downstream: 16-billing-handoff-operations, Contracts | Owners: BSS Rating team -->

# DESIGN — Rated Output, Delta Dedup & Balance Effects (Slice 15, pipeline)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-design-rated-output`

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
  - [4.1 Outcome → RatedCharge / BillableItem Mapping (normative, ratifies slice 11 §4.1)](#41-outcome--ratedcharge--billableitem-mapping-normative-ratifies-slice-11-41)
  - [4.2 Rated-Output Store and prior-rated-version Lineage (normative)](#42-rated-output-store-and-prior-rated-version-lineage-normative)
  - [4.3 Delta Dedup by Correction Key (normative)](#43-delta-dedup-by-correction-key-normative)
  - [4.4 Provisional-FX Supersession (normative)](#44-provisional-fx-supersession-normative)
  - [4.5 CommitmentBalanceEffect Publication (normative)](#45-commitmentbalanceeffect-publication-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

The **persistence and effect-publication** half of the pipeline: it stores every resolved outcome
with its sealed `pricingSnapshotRef` and a `prior-rated-version` lineage for corrections, implements
the **outcome → RatedCharge / BillableItem mapping** (core slice [`11`](./11-consumer-contracts.md)
§4.1 — this slice is the intra-gear **ratifier** ADR-0002 promised, dissolving review-finding C6),
enforces **delta dedup by the correction key** (T-D-11), materializes corrections as immutable
`Adjustment`s, and **publishes `CommitmentBalanceEffect`s** to Contracts per rated outcome (T-D-10).

It is the boundary between the pure evaluation core and the durable financial record. Two invariants
govern it: **immutability** — a `RatedCharge` is never edited; a correction only ever *adds* an
`Adjustment` keyed by the correction key (posted-financial immutability, core slices
[`01`](./01-foundation.md)/[`08`](./08-retroactivity-corrections.md)) — and **idempotent
publication** — a retried delta returns the recorded outcome and is never re-emitted, and a
`CommitmentBalanceEffect` is idempotent on the emitting outcome's key so a redelivery never
double-draws a pool. What leaves here goes to Billing (slice
[`16`](./16-billing-handoff-operations.md)) and Contracts, never back into the core
([`../PRD.md`](../PRD.md) §6.10, §9.2).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| Outcome → RatedCharge/BillableItem mapping (core slice [`11`](./11-consumer-contracts.md) §4.1, ADR-0002 C6) | `OutcomeMapper` maps each resolved line to a `RatedCharge` (full-precision amount + rounding-policy id + sealed ref + `{skuId, planId, priceId}`); obligations ride as envelope, never charges; this slice **ratifies** the §4.1 table (§4.1). |
| `cpt-cf-bss-rating-fr-idempotency` (delta family, T-D-11) | `DeltaDedupIndex` keys every delta by `(unitKey[, slice], prior-rated-version, snapshot)`; a retried key returns the recorded `Adjustment` and re-emits nothing (§4.3). |
| Posted-period protection (PRD §6.10) | The original `RatedCharge` is immutable; a correction materializes a new `Adjustment`; the `prior-rated-version` chain is the correction lineage (§4.2). |
| `CommitmentBalanceEffect` publication (T-D-10) | `BalanceEffectPublisher` publishes per-pool signed draw/refill deltas to Contracts via a transactional outbox, idempotent on the outcome key; Contracts serializes `balanceVersion` and cascades corrections (§4.5). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-resilience` | Dedup + idempotent outbox | A retried outcome/delta is absorbed by the dedup index; balance effects are outbox-emitted at-least-once, idempotent on the outcome key; a crash resumes without double-charge or double-draw | Chaos/retry test |
| `cpt-cf-bss-rating-nfr-horizontal-scale` | Partitioned store | The rated-output store and dedup index are partitioned by the pinned `orderingTenantId`; persistence stays partition-local; balance-effect publication is an async outbox, not a hot-path cross-partition write | Design + load test |
| `cpt-cf-bss-rating-nfr-throughput-latency` | Write path | Persistence + mapping is a single partition-local write per outcome; the balance-effect outbox drains asynchronously | Load test (slice [`16`](./16-billing-handoff-operations.md)) |

#### Key Decisions

| Decision | Summary |
|----------|---------|
| This slice ratifies slice 11 §4.1 | ADR-0002 makes the outcome-mapping table the actual pipeline design here (intra-gear), not a proposal awaiting an external Rating PRD; §4.1 reproduces + confirms it. |
| Rated-output dedup is the second dedup layer | Distinct from ingestion dedup (slice [`12`](./12-usage-ingestion-normalization.md)): `RatedCharge` dedups on the usage key + snapshot, `Adjustment` dedups on the correction key (T-D-11). |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-rob`

```text
rating-core outcomes (via slice 14)   ResolvedPriceOutcome + obligations + sealed pricingSnapshotRef
        ▼
Rated output (this slice)             OutcomeMapper · RatedOutputStore · DeltaDedupIndex ·
        │                             AdjustmentMaterializer · BalanceEffectPublisher
        ▼
Downstream                            Billing handoff (slice 16) · CommitmentBalanceEffect → Contracts
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | Outcome mapping, rated-output persistence, delta dedup, adjustment materialization, balance-effect publication | Rust modules in the `rating` gear (pipeline crate) |
| Domain | `RatedCharge`/`BillableItem`/`Adjustment` shapes, `prior-rated-version` chain, `CommitmentBalanceEffect` | Rust; GTS + Rust domain structs |
| Infrastructure | The **rated-output store**, the **delta-dedup index**, the balance-effect **transactional outbox** | PostgreSQL, SecureORM (`toolkit-db`) |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Immutable charges, additive corrections

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-immutable-charges-rob`

A `RatedCharge` is never edited; a correction is a new `Adjustment` keyed by the correction key, and
the `prior-rated-version` chain records the lineage (posted-financial immutability — core slices
[`01`](./01-foundation.md)/[`08`](./08-retroactivity-corrections.md); PRD §6.10).

#### Idempotent everywhere it publishes

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-idempotent-publish-rob`

Delta dedup returns the recorded outcome on a retried correction key; balance-effect publication is
idempotent on the emitting outcome key; the Billing handoff (slice
[`16`](./16-billing-handoff-operations.md)) is idempotent on the usage/correction key. No retry
double-charges or double-draws (T-D-11, T-D-10).

#### Persist what the core sealed, add nothing

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-persist-sealed-rob`

The `pricingSnapshotRef` is sealed by the core; this slice persists it verbatim and never mints or
edits a segment. Rated output is a faithful record of a core outcome, not a second evaluation.

### 2.2 Constraints

#### Delta-dedup owner: Rating (here)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-delta-dedup-here-rob`

The delta-dedup **owner is Rating**, enforced in this slice — the delta persister (T-D-11; core
slice [`01`](./01-foundation.md) §2.2, slice [`08`](./08-retroactivity-corrections.md) §2.2);
Billing's idempotent `Adjustment` consumption is defense in depth.

#### No money computed, only recorded

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-record-not-compute-rob`

This slice computes no amount — it records the core's full-precision outcome and the rounding-policy
id; rounding and floor/cap execution are Billing's (core slice [`01`](./01-foundation.md) §4.4, slice
[`09`](./09-period-plan-change.md)).

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-rob`

- **`RatedCharge`** — one charge line + unit: full-precision amount, rounding-policy id, sealed `pricingSnapshotRef`, `{skuId, planId, priceId}`, **`glCode`** (the accounting pass-through Finance/Billing post journal entries from — non-null at MVP, slice [`10`](./10-governance-asc606.md) §4.3; carried, never interpreted), discount/FX lineage, ASC refs (null@MVP), the provisional flag (invoice-period FX), the usage/period key; immutable.
- **`Adjustment`** — a correction delta: correction key `(unitKey[, slice], prior-rated-version, snapshot)`, signed full-precision amount, reversal effects, bitemporal stamps; immutable; references the `RatedCharge` version it corrects.
- **`RatedOutputVersion`** — the `prior-rated-version` chain node: which version of a window/unit's rating this outcome is, for deterministic diffing (core slice [`08`](./08-retroactivity-corrections.md)).
- **`CommitmentBalanceEffect`** — per-pool signed draw/refill deltas for one outcome, idempotent on the outcome key; published to Contracts (T-D-10).
- **`BillableItem`** — the Billing-facing envelope carrying the `RatedCharge`/`Adjustment` + obligations for slice [`16`](./16-billing-handoff-operations.md).

### 3.2 Component Model

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-rated-output`

- **`OutcomeMapper`** — maps a `ResolvedPriceOutcome` to `RatedCharge`(s) / `Adjustment`(s) per the §4.1 table; obligations become envelope ride-alongs, never charges.
- **`RatedOutputStore`** — persists outcomes + sealed ref + the `prior-rated-version` chain (§4.2).
- **`DeltaDedupIndex`** — the authoritative delta dedup on the correction key (§4.3).
- **`AdjustmentMaterializer`** — turns a core `reresolve` delta into an immutable `Adjustment`.
- **`BalanceEffectPublisher`** — publishes `CommitmentBalanceEffect`s to Contracts via the transactional outbox, idempotent on the outcome key (§4.5).

### 3.3 API Contracts

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-interface-rated-output-rob`

**Inbound (from slice [`14`](./14-unit-synthesis-period-tick.md) / `rating-core`)**: resolved
outcomes (first rating) and `reresolve` deltas, with the sealed `pricingSnapshotRef` and obligations.
**Outbound**: `RatedCharge`/`Adjustment` + obligations to slice
[`16`](./16-billing-handoff-operations.md) (Billing handoff); `CommitmentBalanceEffect`s to Contracts
(outbox). The `CommitmentBalanceEffect` event schema is a **Contracts/Rating cross-PRD obligation**
to mirror (T-D-10) — the Rating side is specified here.

### 3.4 Internal Dependencies

Upstream: slice [`14`](./14-unit-synthesis-period-tick.md) (delivers core outcomes + routes cascade
`reresolve`), core slices [`01`](./01-foundation.md) (outcome envelope, sealed ref),
[`05`](./05-commitments-reservations.md) §4.1 (balance-effect contract),
[`08`](./08-retroactivity-corrections.md) (correction keys, delta shape). Downstream: slice
[`16`](./16-billing-handoff-operations.md) (Billing delivery). Ratifies the mapping of slice
[`11`](./11-consumer-contracts.md) §4.1.

### 3.5 External Dependencies

| Dependency | What crosses the boundary | Contract |
|------------|---------------------------|----------|
| Contracts | `CommitmentBalanceEffect`s out (per-pool signed deltas); balance-effect cascade triggers back in (via slice 14) | slice [`05`](./05-commitments-reservations.md) §4.1 (T-D-10); cross-PRD event schema |
| Billing (via slice 16) | `RatedCharge`/`Adjustment` + obligations out at full precision | slice [`11`](./11-consumer-contracts.md) §4.6; slice [`16`](./16-billing-handoff-operations.md) |

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-persist-outcome-rob`

**Persist a first-rating outcome**:

1. A resolved outcome arrives from slice [`14`](./14-unit-synthesis-period-tick.md) with its sealed ref + obligations.
2. `OutcomeMapper` produces the `RatedCharge`(s) per §4.1 (incl. zero-due `prepaid_drawdown` lines, provisional invoice-period-FX flag); obligations become envelope ride-alongs.
3. `RatedOutputStore` persists the outcome + sealed ref + `RatedOutputVersion` (partition-local write); the `RatedCharge` dedups on the usage/period key + snapshot.
4. `BalanceEffectPublisher` enqueues the `CommitmentBalanceEffect` on the transactional outbox (same commit as the store write); slice [`16`](./16-billing-handoff-operations.md) delivers the `BillableItem` to Billing.

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-persist-delta-rob`

**Persist a correction delta**:

1. A `reresolve` delta arrives (core slice [`08`](./08-retroactivity-corrections.md)) with its correction key `(unitKey[, slice], prior-rated-version, snapshot)`.
2. `DeltaDedupIndex` checks the key: a known key returns the recorded `Adjustment` and stops (idempotent replay, no re-emit).
3. Else `AdjustmentMaterializer` writes a new immutable `Adjustment` (signed amount, reversal effects, bitemporal stamps) and advances the `prior-rated-version` chain; the original `RatedCharge` is untouched.
4. The `Adjustment` + any per-pool `CommitmentBalanceEffect` (refill/re-draw) publish downstream idempotently on the correction key.

### 3.7 Database Schemas and Tables

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-storage-rated-output-rob`

**Owned (partitioned by the pinned `orderingTenantId`, UTC):**

- `rated_output` — resolved outcome + sealed `pricingSnapshotRef` + `RatedOutputVersion` chain; the `RatedCharge` rows (immutable); dedup index on `(usageKey | period-unit key, snapshot)`.
- `delta_dedup` — unique index on the correction key `(unitKey[, slice], prior-rated-version, snapshot)` (`unitKey` = `usageKey | period-unit key`, mirroring `rated_output`'s generalized key — 2026-07-28 review fix); the authoritative delta dedup. **Never pruned** — it stays hot for the full multi-year correction horizon, exactly like slice [`12`](./12-usage-ingestion-normalization.md)'s `usage_dedup` ("archiving it would silently shrink the dedup guarantee"; the rule was stated there and only implied here — 2026-07-31 review, #43). The `balance_effect_outbox` is drained-by-definition and the counter stores are rebuildable, so they need no such policy.
- `adjustment` — immutable correction deltas (append-only).
- `balance_effect_outbox` — the transactional outbox for `CommitmentBalanceEffect`s (committed with `rated_output`; at-least-once; idempotent on the outcome key).

Concrete DDL is Design. `RATED`/`Adjustment` rows carry full-precision amounts + the rounding-policy id
(never rounded here); no floor/cap or period min/max is applied — that is Billing's (core slice
[`09`](./09-period-plan-change.md)).

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-rob`

Pipeline crate in the one `rating` gear deployable. Persistence is partition-local on the pinned
`orderingTenantId`; the `balance_effect_outbox` drains via a coordinated relay (coordination lease
library, per-shard) — asynchronous, so the balance-effect cross-gear write never blocks the
partition-local rated-output write. First rating and the correction lane (slice
[`14`](./14-unit-synthesis-period-tick.md)) both land here; dedup keeps re-resolutions idempotent.

## 4. Additional Context

### 4.1 Outcome → RatedCharge / BillableItem Mapping (normative, ratifies slice 11 §4.1)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-outcome-mapping-rob`

This slice **ratifies** the core slice [`11`](./11-consumer-contracts.md) §4.1 mapping (ADR-0002
dissolves the external-ratifier open — it is now intra-gear):

| Outcome element | Mapping (here) |
|---|---|
| resolved line (usage / recurring / `capacityCharge`) | one `RatedCharge` per charge line + unit: full-precision amount + rounding-policy id; `BillableItem.pricingSnapshotRef` = the sealed ref; `{skuId, planId, priceId}` verbatim |
| zero-due `prepaid_drawdown` in-commit line (core slice [`05`](./05-commitments-reservations.md) §4.1, T-D-14) | a `RatedCharge` with amount-due 0 + notional value in lineage — **itemized, never dropped** |
| obligations (`TrueUpObligation`, `PeriodFloorCapObligation`) | **envelope ride-along** to Billing execution — never a `RatedCharge` |
| provisional invoice-period-FX amount (core slice [`07`](./07-currency-fx.md)) | `RatedCharge` flagged **provisional**; the close-time delta supersedes by correction key, never mutates (§4.4) |
| correction deltas (core slice [`08`](./08-retroactivity-corrections.md)) | immutable `Adjustment` keyed by the correction key; the original `RatedCharge` is immutable |
| idempotency | usage/period key ⇒ `RatedCharge` dedup; correction key ⇒ `Adjustment` dedup — both enforced **here** (T-D-11) |
| `CommitmentBalanceEffect` (core slice [`05`](./05-commitments-reservations.md) §4.1, T-D-10) | published to Contracts per rated outcome, idempotent on the outcome key (§4.5) |

Incompatible changes to this mapping take a **major version bump** (PRD §9.1); the mapping is now
stable design, not a proposal.

### 4.2 Rated-Output Store and prior-rated-version Lineage (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-rated-store-rob`

- Every outcome persists with its **sealed `pricingSnapshotRef`** and a `RatedOutputVersion` — the `prior-rated-version` the core diffs a correction against (core slice [`08`](./08-retroactivity-corrections.md) §4.2). The store is the authoritative rated record; loss of any downstream cache is repaired by replay from it + the pinned snapshot.
- A `RatedCharge` is **immutable**: a correction never edits it; it materializes a new `Adjustment` and advances the version chain. The chain of `(RatedOutputVersion, correction key)` is a window/unit's full rating lineage.
- Snapshots referenced by rated output MUST remain resolvable for the full correction horizon (posted-period corrections arrive years later — the ≥ 7-year retention of core slice [`08`](./08-retroactivity-corrections.md) §4.1); this slice retains the sealed ref, pricing retains the snapshot content.

### 4.3 Delta Dedup by Correction Key (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-delta-dedup-rob`

- The **delta-dedup owner is Rating**, enforced here as the delta persister (T-D-11; core slice [`01`](./01-foundation.md) §2.2, slice [`08`](./08-retroactivity-corrections.md) §2.2/§4.2). `DeltaDedupIndex` is a unique index on `(unitKey[, slice], prior-rated-version, snapshot)` — `unitKey` spanning both unit families like `rated_output` (a retried close-time FX re-rate of a recurring line collides here, 2026-07-28 review fix).
- A retried correction key **returns the recorded `Adjustment`** and re-emits nothing downstream; a genuinely new correction to the same unit diffs against the then-current `prior-rated-version`, producing a **new** key (the keys never repeat, and the chain is the correction lineage).
- Billing additionally treats `Adjustment` consumption as idempotent on the same key — **defense in depth**, not the primary guard (core slice [`08`](./08-retroactivity-corrections.md) §2.2).

### 4.4 Provisional-FX Supersession (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-provisional-fx-rob`

- An invoice-period-FX line is persisted as a **provisional** `RatedCharge` (core slice [`07`](./07-currency-fx.md) §4.2); at period close the close-time re-rate arrives as a `reresolve` delta (a full re-execution of step 8 + the billing-currency coupon pass — core slice [`07`](./07-currency-fx.md) §3.6) under the correction key.
- The delta **supersedes** the provisional amount via a new `Adjustment` — the provisional `RatedCharge` is **never mutated**; Billing sees the provisional line then its close-time adjustment, both immutable, both keyed. The provisional flag on the original + the correction key on the delta let Billing reconcile without re-computation.

### 4.5 CommitmentBalanceEffect Publication (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-balance-effect-rob`

- Per rated outcome that **observed** a commitment pool — drew, refilled, **or was rated overage against it without drawing (a zero-draw effect carrying an overage marker — T-D-27, 2026-08-01)** — this slice publishes a **`CommitmentBalanceEffect`** (per-pool signed draw/refill deltas) to Contracts via the `balance_effect_outbox`, **idempotent on the emitting outcome's evaluation/correction key** (T-D-10; core slice [`05`](./05-commitments-reservations.md) §4.1). A redelivery never double-draws. The observation coverage is what makes T-D-10's cascade obligation satisfiable: this stream is Contracts' only source of Rating unit identity, and a unit rated pure overage against an exhausted pool previously published nothing — unnameable by the very cascade that owed it a re-price (2026-07-31 review, #7).
- **Contracts serializes** per-pool `balanceVersion`, applies effects in received order, and freezes `(balance, balanceVersion)` for the next context assembly (slice [`14`](./14-unit-synthesis-period-tick.md) §4.5) — so the cross-unit ordering the core depends on is Contracts', **off the rating hot path**; rating's obligation ends at publishing the per-pool effect.
- A balance-affecting correction cascades: Contracts emits re-resolution triggers for every later-`balanceVersion` unit that drew or was rated overage against the pool; slice [`14`](./14-unit-synthesis-period-tick.md) routes them, coalesced and bounded (§4.4 there), and the resulting deltas land back here — deduped (§4.3), each under its own pin.
- The `CommitmentBalanceEffect` **event schema** is a Contracts/Rating cross-PRD obligation to mirror (T-D-10); the Rating-side shape (per-pool signed deltas, outcome-key idempotency, draw order) is fixed here — the Contracts-side application/serialization is theirs.

## 5. Traceability

- **PRD**: §9.2 (Rating handoff), §6.1 (`fr-idempotency` delta family), §6.10 (delta-only corrections, posted immutability), §7.1 (scale/resilience NFRs).
- **Seams**: M7 (rated aggregate lineage), S1 (sealed ref persisted), B1 (bundle component lineage addressable in rated output) — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-10 (balance effects + cascade), T-D-11 (delta dedup owner = Rating, here), T-D-14 (zero-due prepaid line), T-D-16 (consolidation; ratifies slice 11 §4.1) — [`../DECISIONS.md`](../DECISIONS.md).
- **ADR**: [`../ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md`](../ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md).
- **Related slices**: [`11-consumer-contracts.md`](./11-consumer-contracts.md) §4.1 (the mapping this slice ratifies), [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) §4.2 (correction keys), [`05-commitments-reservations.md`](./05-commitments-reservations.md) §4.1 (balance-effect contract), [`07-currency-fx.md`](./07-currency-fx.md) (provisional FX), [`14-unit-synthesis-period-tick.md`](./14-unit-synthesis-period-tick.md) (outcomes in, cascades), [`16-billing-handoff-operations.md`](./16-billing-handoff-operations.md) (Billing delivery).
