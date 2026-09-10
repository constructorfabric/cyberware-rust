Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Retroactivity & Corrections (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Rating, Billing, Pricing (Product Catalog), Contracts | Downstream: Rating, Billing | Owners: BSS Rating team -->

# DESIGN — Retroactivity & Corrections (Slice 8)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-design-retroactivity-corrections`

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
  - [4.1 Snapshot-Only Replay (normative)](#41-snapshot-only-replay-normative)
  - [4.2 Correction Key and Delta Idempotency (normative)](#42-correction-key-and-delta-idempotency-normative)
  - [4.3 periodState Routing (normative)](#43-periodstate-routing-normative)
  - [4.4 Reversal Math and Emission Guards (normative)](#44-reversal-math-and-emission-guards-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

Retroactivity is **not a step**: this slice is the correction/replay surface that **wraps** the
per-line §17.1 flow ([`./01-foundation.md`](./01-foundation.md) §3.4) — late-arriving usage,
correcting/negative usage, the slice-07 FX close re-rate, and plan-change portion corrections. A
correction is **the same pure pipeline, run again**: replay steps 1–9 over the **snapshot pinned at
first rating of that unit** with corrected frozen inputs (an administrative re-rate substitutes
the superseding snapshot — §4.1), diff against the prior rated version, and
emit **only deltas** under the correction key `(unitKey[, slice], prior-rated-version, snapshot)`. A live catalog
read on a correction path is *a defect by definition* (SEAMS W2), identically for open and posted
periods; pricing guarantees snapshot retention for open windows with no read-model change.

`periodState` routes the destination, never the math: `open` re-resolves in place (deltas for
already-rated events); `closed_posted` sends the same diff through posted-period protection — delta
adjustments via the Adjustment path, posted invoices immutable; missing `periodState` fails closed.
Reversal is diff-derived: Rating re-materializes `Q` before replay, the re-run waterfall yields the
pool refill, and no resolved line is ever driven negative. The wrapper is side-effect-free: Rating
never mutates Usage, counters, balances, or posted financials — the owners apply the effects ([`../PRD.md`](../PRD.md) §6.10, §6.1).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-fr-posted-period-protection` | `periodState = closed_posted` ⇒ the `PostedPeriodGuard` routes the diff to delta-only Adjustment output (§4.3); posted invoice lines and prior rated outputs are never mutated; every retro delta carries the bitemporal stamps — usage-observation time and pricing-policy decision time (§4.4). |
| `cpt-cf-bss-rating-fr-late-arriving-usage-reresolve` | For `tierAggregationWindow != per_event` with `periodState = open`, the `ReplayCoordinator` re-runs the **whole window unit** strictly from the pinned snapshot after Rating re-materializes `Q` — the **administrative re-rate (T-D-21)** being the one sanctioned exception, replaying over the *superseding* snapshot because the pin itself is what a policy correction fixes (§4.1); the `DiffEngine` emits deltas for already-rated events only (§4.1, §4.3); missing `periodState` fails closed. |
| `cpt-cf-bss-rating-fr-usage-corrections` | A correcting/negative usage event reverses deterministically: replay over the corrected `Q` re-places tiers and re-runs the step-6 waterfall — the drawdown difference *is* the pool refill; compensating deltas are emitted and no resolved line goes negative (§4.4). Correction ingestion and dedup remain Rating's. |
| `cpt-cf-bss-rating-fr-idempotency` (delta family) | Every delta carries the correction key `(unitKey[, slice], prior-rated-version, snapshot)` (`unitKey` = usage counter key or period-driven key — [`./01-foundation.md`](./01-foundation.md) §4.2); a retry replays to the byte-identical delta set and never double-adjusts; the delta-dedup **owner is Rating** (§2.2, T-D-11). |
| `cpt-cf-bss-rating-fr-separation` | The wrapper is side-effect-free: outcomes leave as deltas via the Adjustment path; Usage is never mutated; balance/counter effects are applied by their owners (Contracts, Rating). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-horizontal-scale` | Partition-key serialization | Concurrent re-resolve for one unit serializes on `(subscription, meter, dimensionKey, window)`; unrelated windows replay in parallel; zero cross-partition locks | Design + load test |
| `cpt-cf-bss-rating-nfr-resilience` | Correction key + fail-closed routing | Retries replay the same key to the same deltas; missing `periodState` / pinned snapshot / prior rated version ⇒ fail closed, never a guess | Chaos/retry test |
| `cpt-cf-bss-rating-nfr-throughput-latency` | Off-hot-path corrections | Replay runs on the correction path (batch / Adjustment), never blocking first rating; the hot path is untouched | Load test; **targets provisional — NFR workshop** ([`../PRD.md`](../PRD.md) §7.1) |
| Bitemporal correction audit (§6.10) | Delta envelope | Usage-observation time + pricing-policy decision time stamped separately on every retro delta; persisted by the delta's consumer (rating-core stores nothing — §3.7) | Joint fixture |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-bss-rating-adr-scope-key-adoption` | Replay selects on the same adopted 8-axis key resolved from the pinned snapshot — a correction can never resolve a different row than first rating did over identical inputs. |
| `cpt-cf-bss-pricing-adr-pricewindow-consolidation` (adopted) | `PriceWindow*` events only invalidate the non-authoritative cache; corrections never consult live window state — expired/cancelled windows are immutable pricing history and the pin is the only read ([`../../../pricing/docs/design/07-pricewindow-linkage.md`](../../../pricing/docs/design/07-pricewindow-linkage.md)). |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-rtr`

```text
Correction wrapper (this slice)   PostedPeriodGuard · ReplayCoordinator · DiffEngine ·
        │  (wraps §17.1 steps 1–9;         ReversalCalculator · BitemporalStamper
        ▼   no step slot of its own)
Evaluation pipeline (Foundation)  reresolve over the pinned snapshot · DeterminismGuard
        │                         (correction keys, serialization) · EmissionGuard
        ▼
Frozen inputs (external SoRs)     re-materialized Q + prior rated versions (Rating) · pinned
                                  snapshot, retained (pricing) · periodState (Billing) · frozen pool set (Contracts)
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | The correction wrapper around the per-line flow; replay triggers from Rating / period close | Rust module in the `rating` gear (rating-core crate) |
| Domain | Correction key, delta envelope, reversal effects, `periodState` routing | Rust; GTS + Rust domain structs |
| Infrastructure | **None authoritative** — corrections read the pin, not the cache; deltas persist with their consumer (Rating/Billing) | Rating persistence |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Replay the pin, never the catalog

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-pinned-replay-rtr`

Every correction replays the snapshot pinned at first rating of that window — never a live catalog
read; a live read on a correction path is a defect by definition (SEAMS W2), for open and posted periods alike.

#### Deltas out, never mutation

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-delta-only-rtr`

The output of every correction is a set of compensating deltas via the Adjustment path; prior rated
outputs and posted invoices are immutable diff inputs, never write targets ([`../PRD.md`](../PRD.md) §6.10, §6.1 `fr-separation`).

#### One math, run twice

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-same-math-rtr`

A correction is the unmodified §17.1 pipeline over corrected frozen inputs — no special-cased retro
formulas. The delta is *defined* as (re-resolved outcome − prior rated version): reversal is deterministic by construction and replay byte-identical ([`./01-foundation.md`](./01-foundation.md) §4.2).

### 2.2 Constraints

#### Posted-period immutability

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-posted-immutability-rtr`

`periodState = closed_posted` ⇒ delta-only corrections via the Adjustment path; posted invoice lines
and prior rated output are never mutated ([`../DESIGN.md`](../DESIGN.md) §2.2 `cpt-cf-bss-rating-constraint-posted-immutability`; [`../PRD.md`](../PRD.md) §6.10).

#### periodState is a required input

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-periodstate-required-rtr`

Billing is the only source of `periodState`; a correction with `periodState` missing fails closed — no guessing ([`../PRD.md`](../PRD.md) §6.10).

#### Delta dedup owner: Rating

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-delta-dedup-owner-rtr`

The correction key makes deltas idempotent; the **owner of delta dedup is Rating** (**decided
2026-07-11, T-D-11** — the Foundation decision, restated here where the deltas are produced:
[`./01-foundation.md`](./01-foundation.md) §2.2; [`../PRD.md`](../PRD.md) §6.1). Rating persists
delta outcomes keyed by the correction key; a retried delta with a known key returns the recorded
outcome and is never re-emitted downstream; Billing additionally treats Adjustment consumption as
idempotent on the same key (defense in depth).

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-rtr`

- **`CorrectionKey`** — `(unitKey[, slice], prior-rated-version, snapshot)` ([`./01-foundation.md`](./01-foundation.md) §4.2; `unitKey` = the usage counter key or the period-driven key, matching `rated_output`) — the slice coordinate present iff a slice-09 split partitions a usage window (never on period-driven units); the identity of every delta.
- **`CorrectionTrigger`** — what entered the wrapper: late usage, correcting/negative usage, FX close re-rate ([`07-currency-fx.md`](./07-currency-fx.md) §3.6), a plan-change portion correction ([`09-period-plan-change.md`](./09-period-plan-change.md)), or an **administrative re-rate** (§4.1 — the one trigger that substitutes a superseding snapshot); all take the same replay path.
- **`PriorRatedVersion`** — frozen input: the rated output version being corrected (Rating persistence).
- **`PeriodState`** — frozen Billing input (`open` \| `closed_posted`); the routing key (§4.3).
- **`DeltaAdjustment`** — the emitted envelope: correction key, signed full-precision per-line amount deltas, discount/FX lineage, reversal effects, bitemporal stamps (`usageObservedAt`, `policyDecidedAt`), and the route taken.
- **`ReversalEffect`** — diff-derived effects of a reversal: per-pool commitment refill (Contracts SoR applies to its balances) and the tier-counter decrement already realized in the re-materialized `Q` (Rating single-writer).

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-correction-wrapper-rtr`

- **`ReplayCoordinator`** — pins `(window, snapshot)`, acquires per-partition serialization, requires Rating's re-materialized `Q`, and invokes the Foundation `reresolve`; fails closed on a missing pin or prior version.
- **`PostedPeriodGuard`** — reads `periodState` and routes (§4.3): `open` ⇒ in-place re-resolution deltas; `closed_posted` ⇒ the same diff through posted-period protection; missing ⇒ fail closed.
- **`DiffEngine`** — computes (re-resolved − prior rated version) per line at full precision; an empty diff emits nothing (idempotent replay).
- **`ReversalCalculator`** — derives `ReversalEffect`s from the step-3/step-6 diffs (tier re-placement, waterfall drawdown difference = refill) and enforces the non-negative guard on every corrected line (§4.4).
- **`BitemporalStamper`** — stamps usage-observation and pricing-policy decision time on every delta (via the Foundation `MetadataRecorder` envelope).

### 3.3 API Contracts

This slice **implements** the Foundation re-resolution contract
`cpt-cf-bss-rating-interface-reresolve-fnd`: `reresolve(window, pinnedSnapshotRef, priorRatedVersion)
→ deltas` — strictly snapshot-replayed, serialized on the partition key, emitting only deltas
([`./01-foundation.md`](./01-foundation.md) §3.3). No second entry point exists: every trigger in §3.1 funnels into it.

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-delta-envelope-rtr`

The **delta adjustment envelope**: every emitted delta carries the correction key, signed
full-precision amounts, reversal effects, discount/FX lineage, and the bitemporal stamps —
consumable by Billing per immutability rules ([`../PRD.md`](../PRD.md) §6.10). The Rating handoff
shape and the Billing `periodState`/obligation contract are owned by
[`11-consumer-contracts.md`](./11-consumer-contracts.md).

### 3.4 Internal Dependencies

Foundation ([`01-foundation.md`](./01-foundation.md)): the `reresolve` contract, correction keys,
`DeterminismGuard` serialization, emission guards. Replay runs the step evaluators of slices
[`02`](./02-selection-eligibility.md)–[`07`](./07-currency-fx.md) unchanged:
[`03-metering-models.md`](./03-metering-models.md) re-places tiers over the corrected `Q`;
[`05-commitments-reservations.md`](./05-commitments-reservations.md) re-runs the waterfall whose
drawdown difference is the refill; [`06-coupons.md`](./06-coupons.md) / [`07-currency-fx.md`](./07-currency-fx.md)
replay coupons and FX — and slice 07's invoice-period close re-rate enters here as a
`CorrectionTrigger`. Plan-change corrections to an already-rated portion
([`09-period-plan-change.md`](./09-period-plan-change.md), §17.2) route through the same keys; boundary contracts: [`11-consumer-contracts.md`](./11-consumer-contracts.md).

### 3.5 External Dependencies

| Dependency | What arrives frozen | Contract |
|------------|--------------------|----------|
| Rating pipeline (intra-gear — slices 12/13/15) | re-materialized `Q` (single-writer per partition key), prior rated versions, usage dedup, correction ingestion | PRD §9.2 handoff; slices 12–15 |
| Billing | `periodState` (`open` / `closed_posted`); consumes delta adjustments per immutability rules | PRD §9.2 Billing |
| Pricing (Product Catalog) | the pinned snapshot, retained for open windows (no live read — W2); window history immutable | [`../SEAMS.md`](../SEAMS.md) W2; pricing [`design/07`](../../../pricing/docs/design/07-pricewindow-linkage.md) |
| Contracts | the commitment pool set, balances, and draw order frozen in `pricingSnapshotRef` (§17.1 step 6); balance SoR applying refill effects | PRD §6.6, §6.10 |

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-open-period-correction-rtr`

**Open-period late arrival** (extends `cpt-cf-bss-rating-flow-reresolve-open-window-fnd`):

1. Late/corrected usage lands in Rating (ingestion + dedup are Rating's); the window's `Q` is re-materialized (single-writer).
2. `ReplayCoordinator` serializes on `(subscription, meter, dimensionKey, window)`; loads the pinned snapshot ref and prior rated version; either missing ⇒ fail closed.
3. Foundation `reresolve` replays steps 1–9 for the **whole window unit** over the pinned snapshot — no live catalog read.
4. `DiffEngine` emits per-line deltas keyed `(unitKey[, slice], prior-rated-version, snapshot)` for already-rated events; an empty diff emits nothing.
5. `BitemporalStamper` + `EmissionGuard`; deltas leave via the Adjustment path.

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-flow-posted-period-correction-rtr`

**Posted-period correction** (same math, protected route):

1. Steps 1–4 of the open-period flow run identically — `periodState` never changes the math (§4.3).
2. `PostedPeriodGuard` routes the diff through posted-period protection: delta adjustments consumable by Billing per immutability rules; posted invoice lines and prior rated output untouched.
3. Every delta records usage-observation time and pricing-policy decision time separately ([`../PRD.md`](../PRD.md) §6.10).
4. A retry replays the same correction key byte-identically; double-adjust is excluded by delta dedup (owner Rating — §2.2, T-D-11).

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-negative-usage-reversal-rtr`

**Correcting / negative usage reversal**:

1. The correcting event passes Rating ingestion/dedup; Rating re-materializes `Q` for the partition key (the tier-counter decrement is realized here — Rating never writes a counter).
2. Replay over the corrected `Q`: tier placement re-resolves (slice 03); the step-6 waterfall re-runs over the snapshot-frozen pool set (slice 05) — the drawdown difference is the commitment-pool refill, applied by Contracts to its balances.
3. `ReversalCalculator` verifies no **resolved corrected line** is negative ([`./01-foundation.md`](./01-foundation.md) §4.4); compensating deltas themselves are signed and may be negative-valued.
4. `periodState` routes the result: `open` ⇒ in-place deltas; `closed_posted` ⇒ the posted-period flow above.

### 3.7 Database Schemas and Tables

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-storage-none-rtr`

**None owned.** Prior rated versions, `Q`, and dedup state live in Rating; snapshot retention is the
pricing gear's W2 obligation; pool balances live in Contracts; `periodState` in Billing. The bitemporal
audit stamps ride the delta envelope into the consumer's persistence — rating-core holds no correction
ledger. Corrections bypass the Foundation's resolved-window cache: they read the pin.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-rtr`

Runs in the `rating` gear (rating-core crate) ([`./01-foundation.md`](./01-foundation.md) §3.8)
— no additional topology. The correction path is off the hot path (batch / Adjustment), serialized per partition key, and safely restartable: a crashed correction replays the same key.

## 4. Additional Context

### 4.1 Snapshot-Only Replay (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-snapshot-replay-rtr`

- The replay source is **the window's pin of record** — the snapshot pinned at first rating, advanced **only** by a T-D-21 administrative re-rate to the superseding snapshot (T-D-24) — for open-period late arrival **and** posted-period corrections alike. A live catalog read on a correction path is a defect by definition (SEAMS W2, resolved 2026-07-10; [`../PRD.md`](../PRD.md) §6.10).
- Pricing guarantees **snapshot retention for open windows** and changes nothing in its read model; the rejected alternative — a pricing historical-window query surface — is recorded in [`../SEAMS.md`](../SEAMS.md) (more cost, weaker determinism).
- `fxTableVersion` at replay: which version applied at which stage is part of the replay inputs ([`../PRD.md`](../PRD.md) §6.9). The FX close re-rate substitutes **only** the close-time version ([`07-currency-fx.md`](./07-currency-fx.md) §4.2); every other frozen input is identical.
- **Administrative re-rate (the sanctioned snapshot substitution — T-D-21)**: an administrative repricing ([`../PRD.md`](../PRD.md) §6.10 — a *policy* decision, not an input correction) replays over the **superseding snapshot** published through the pricing governance pipeline (corrective publish / historical import — always-material, two-person on the pricing side); the diff still runs against the prior rated version, the correction key carries the **new** snapshot (the key shape already admits it), and `policyDecidedAt` is the corrective-publish instant. **The re-rate advances the window's pin of record (T-D-24, 2026-08-01 — flagged for veto)**: after it, a later **input** correction replays the **superseding** pin and diffs against the then-current rated version — before T-D-24, "input corrections replay the original pin" meant a late three-unit usage record after a corrective publish computed *(original pin + late usage) − (re-rated version on the superseding pin)*, silently bundling the **negation of the two-person-approved repricing** into one signed delta. Input corrections replay the pin of record (the original pin until a re-rate has run); policy corrections replay the superseding pin — never a live read in either case.
- **Administrative re-rate: trigger, enumeration, bound (2026-07-31 review, #49)**: the re-rate is an **operator-initiated run** (`rerate × execute` under the slice-[`10`](./10-governance-asc606.md) authz catalog — never automatic on a corrective publish), and its scope is enumerable by construction: **every unit whose pin of record references the superseded snapshot** within the correction horizon, resolved from the rated-output store's version chains (slice [`15`](./15-rated-output-balance-effects.md)). The run enumerates the affected units up front and surfaces the count for operator confirm (the blast radius is every subscription pinned to the superseded snapshot); execution is ordinary deltas under §4.2 keys, journaled and resumable, coalesced as one cascade generation (slice [`14`](./14-unit-synthesis-period-tick.md) §4.4). Termination is structural — a finite enumerated set.
- **Retention horizon**: pinned snapshots MUST remain resolvable for the full correction horizon (posted-period corrections arrive years later) — satisfied by the pricing gear's append-only in-table history + monotonic read-model versions (≥ the 7-year audit retention).

### 4.2 Correction Key and Delta Idempotency (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-correction-key-rtr`

- Every delta carries the correction key `(unitKey[, slice], prior-rated-version, snapshot)` ([`./01-foundation.md`](./01-foundation.md) §4.2 — `unitKey` = the usage counter key **or** the period-driven key, so recurring-line FX re-rates and true-up recomputes dedup exactly like usage corrections): a retry **replays** — same key, byte-identical delta set — and never double-adjusts.
- A subsequent correction to the same window diffs against the then-current prior rated version, producing a **new** key: keys never repeat across distinct corrections, and the chain of keys is the window's correction lineage.
- The key makes dedup *possible*; the dedup **owner is Rating** (§2.2, T-D-11) — enforcement lives with the delta persister.

### 4.3 periodState Routing (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-periodstate-routing-rtr`

- `open` ⇒ re-resolve in place: deltas for already-rated events; the re-resolved window outcome stands for subsequent rating in the window ([`../PRD.md`](../PRD.md) §6.10).
- `closed_posted` ⇒ the **same diff** through posted-period protection: delta adjustments via the Adjustment path; posted invoice lines immutable ([`./01-foundation.md`](./01-foundation.md) §3.6 step 4).
- missing ⇒ **fail closed**, no guessing.
- **Close fence (T-D-28, 2026-08-01 — joint with Billing, flagged for veto)**: Billing's periodState transitions carry a per-`(subscription, period)` **sequence**; the slice-[`16`](./16-billing-handoff-operations.md) relay keeps **highest-wins** (plain forward-only would be wrong — a re-opened window is legitimate, [`09`](./09-period-plan-change.md) §4.2), and routing **re-reads the projection at route time**, recording the observed `(periodState, sequence)` in the delta's audit trail. Before T-D-28 the one frozen context input with no version had no monotonicity rule and no fence — routing on a stale `open` across a close produced the wrong artifact kind and audit posture (mis-routing only: the double-billing reading is excluded by §4.2 dedup + Billing idempotency; 2026-07-31 review, #20/#48).
- Routing decides the destination and audit posture only — never the math (§2.1, one math run twice).

### 4.4 Reversal Math and Emission Guards (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-reversal-guards-rtr`

- **Reversal is diff-derived**: the commitment-pool refill is (prior drawdown − re-resolved drawdown) per pool, in the waterfall order frozen in `pricingSnapshotRef`; the tier-counter decrement is realized in the re-materialized `Q` before replay (Rating single-writer, key `(subscription, meter, dimensionKey, window)` — SEAMS M7). Rating computes effects; owners apply them (Contracts balances, Rating counters).
- **Cascades (T-D-10, T-D-12)**: two frozen inputs couple evaluation units, and correcting one unit re-resolves its dependents as ordinary deltas, each under its **own** pin: (a) a balance-affecting correction changes later units' frozen pool balances — Contracts (the `balanceVersion` serializer) emits re-resolution triggers for every later unit that drew, or was rated overage against, the affected pool ([`05-commitments-reservations.md`](./05-commitments-reservations.md) §4.1); (b) an earlier sub-window slice's `Q` change shifts later slices' `bandOffsetQ` — Rating re-materializes the offsets and the later slices of the same aggregation window re-resolve ([`03-metering-models.md`](./03-metering-models.md) §4.3). Cascade deltas carry their own correction keys; termination is structural — both chains are finite and strictly ordered (balance versions / slice order).
- **Non-negative**: no resolved corrected line goes negative ([`./01-foundation.md`](./01-foundation.md) §4.4); the guard applies to resolved line outcomes, not to delta signs — compensating deltas are legitimately negative-valued. The clamp-vs-credit residue policy remains the PRD §15 open (Finance).
- **Bitemporal audit**: usage-observation time and pricing-policy decision time are recorded **separately** on every retro delta ([`../PRD.md`](../PRD.md) §6.10).
- **Full precision**: deltas leave at full intermediate precision; Billing aggregates and rounds ([`./01-foundation.md`](./01-foundation.md) §4.4).

## 5. Traceability

**Traces to**: `cpt-cf-bss-rating-fr-posted-period-protection`, `cpt-cf-bss-rating-fr-late-arriving-usage-reresolve`, `cpt-cf-bss-rating-fr-usage-corrections`

- **PRD**: §6.10 (`fr-posted-period-protection`, `fr-late-arriving-usage-reresolve`, `fr-usage-corrections`), §6.1 (`fr-idempotency` delta family, `fr-separation`), §17.1 "Determinism and Rating compatibility", §12 AC 9–10, §15 (clamp-vs-credit open), §9.2 (Rating / Billing contracts), §7.1 NFRs.
- **Seams**: W2 (owned — snapshot-only replay), M7 (counter key), S1 (the pinned ref is the replay source) — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-04 (primary — pinned-snapshot replay + counter key), T-D-03 (snapshot composition), T-D-10/T-D-12 (cascade re-resolution), T-D-11 (delta dedup: Rating) — [`../DECISIONS.md`](../DECISIONS.md).
- **Slices**: [`01-foundation.md`](./01-foundation.md) (pipeline, `reresolve` interface, correction keys, guards), [`03-metering-models.md`](./03-metering-models.md) (tier re-placement), [`05-commitments-reservations.md`](./05-commitments-reservations.md) (pool refill semantics), [`06-coupons.md`](./06-coupons.md) / [`07-currency-fx.md`](./07-currency-fx.md) (coupon/FX replay; FX close delta), [`09-period-plan-change.md`](./09-period-plan-change.md) (plan-change correction deltas, period obligations), [`11-consumer-contracts.md`](./11-consumer-contracts.md) (Rating/Billing boundary).
- **Pricing design set**: [`07-pricewindow-linkage.md`](../../../pricing/docs/design/07-pricewindow-linkage.md) (immutable window history, `PriceWindow*` events), [`06-consumer-contracts.md`](../../../pricing/docs/design/06-consumer-contracts.md) (frozen read-model consumer contract).
