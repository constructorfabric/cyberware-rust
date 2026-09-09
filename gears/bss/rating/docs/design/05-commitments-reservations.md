Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Commitments & Reservations (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Contracts & Agreements, Pricing (Product Catalog), OSS (reservation entitlement), Subscriptions | Downstream: Rating, Billing (obligation execution) | Owners: BSS Rating team -->

# DESIGN — Commitments & Reservations (Slice 5)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-design-commitments-reservations`

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
  - [4.1 Commitment-Pool Waterfall (normative)](#41-commitment-pool-waterfall-normative)
  - [4.2 Reservation Flavors and Pool Precedence (normative)](#42-reservation-flavors-and-pool-precedence-normative)
  - [4.3 Reserved-Rate Two-Source Rule (normative)](#43-reserved-rate-two-source-rule-normative)
  - [4.4 Commitment Pool vs Prepaid Credit Grant (normative)](#44-commitment-pool-vs-prepaid-credit-grant-normative)
  - [4.5 Obligations, Reversals, and Period Boundaries (normative)](#45-obligations-reversals-and-period-boundaries-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice is the **step-6 evaluator** of the §17.1 order: the commercial-composition layer between the
overlay/contract steps (4–5) and coupons (step 7), registered into the Foundation pipeline's fixed slot
([`01-foundation.md`](./01-foundation.md) §3.2). Per **T-D-05**, `committed` is **not a modelKind**:
commitment and reservation *wrap* the model output of steps 2–5 — they split the evaluation unit into
reserved / in-commit / overage portions priced at already-resolved rates, never a formula of their own
([`../DECISIONS.md`](../DECISIONS.md), [`../PRD.md`](../PRD.md) §6.2).

Ownership is deliberately thin: **Contracts is the SoR for `commitmentPools[]` balances**, the reservation
entitlement lifecycle is OSS/Contracts', self-service reserved rates are the pricing gear's snapshot
attributes. Rating evaluates a **frozen pool/reservation snapshot** carried in the context, records the
drawdown effect, and **surfaces** the structured `TrueUpObligation` — it never stores a balance, posts a
charge, or mutates contract state ([`../PRD.md`](../PRD.md) §6.6, [`../SEAMS.md`](../SEAMS.md) ownership
matrix). The slice owns two seams: **M8** (pool ≠ prepaid credit grant, §4.4) and **M9** (§4.3).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-fr-commitment-drawdown` | Deterministic waterfall over the ordered `commitmentPools[]` snapshot (§4.1): each pool absorbs quantity/spend up to its remaining frozen balance; residual is overage/on-demand; single pool is the default special case; always step 6, no reordering. |
| `cpt-cf-bss-rating-fr-committed-usage` | The in-commit / overage split prices at the distinct frozen rates; period true-up per contract clause is surfaced as a structured `TrueUpObligation` (§4.5) — never an implicit posted charge; reversal (pool refill) is delegated to slice 08. |
| `cpt-cf-bss-rating-fr-reservation-consumption-flavor` | The `ReservationMatcher` splits matched vs remainder **before** pools (§4.2): matched usage prices at the reserved rate (source per §4.3), the remainder at the steps-2–5 on-demand rates with its tier counter banded from zero; matched quantity never draws pools. |
| `cpt-cf-bss-rating-fr-capacity-charge` | Capacity flavor emits `capacityCharge = reservedRate × reservedQuantity` regardless of measured usage (§4.2); never reduced by absent usage, never draws `commitmentPools[]`; flavor/quantity/rate frozen in the snapshot. |
| `cpt-cf-bss-rating-fr-hybrid-pricing` | On a hybrid plan the commitment attaches to the **usage line** unless the plan marks it plan-level; the attachment configuration is consumed frozen from `pricingSnapshotRef` — this slice never decides attachment. |
| `cpt-cf-bss-rating-fr-evaluation-order` | A registered step evaluator in the compiled §17.1 slot 6; intra-step order (reservation → waterfall → overage) is equally fixed (§3.6). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-throughput-latency` | Step-6 evaluator | Pure in-process arithmetic over the context-frozen pool/reservation snapshot; no Contracts/OSS call on the hot path | Load test; targets provisional (NFR workshop, [`../PRD.md`](../PRD.md) §7.1) |
| `cpt-cf-bss-rating-nfr-horizontal-scale` | Partitioning contract | Per-unit evaluation over balances frozen into the context; no live balance read, no cross-partition pool lock (cross-unit balance sequencing is the SoR's — resolved 2026-07-11, T-D-10 `balanceVersion` serialization, §4.1) | Design + load test |
| `cpt-cf-bss-rating-nfr-resilience` | Fail-closed guards | Missing/torn pool snapshot, a `reservationMatch` without a resolvable rate source, or an unknown flavor fails closed — never a guessed drawdown | Chaos/retry + joint fixtures |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-bss-rating-adr-scope-key-adoption` | The rates step 6 composes over — base row, overlay stack, and the self-service `reservedRate` attribute riding the selected usage row — all resolve on the adopted 8-axis key; this slice adds no selection axis. |

No slice-local ADR: step-6 semantics are PRD-normative (§6.6, §17.1) under **T-D-05** / **T-D-08**.

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-cmt`

```text
Step-6 evaluator (this slice)   ReservationMatcher → PoolWaterfall → TrueUpAssembler
        │  (registers into the fixed §17.1 slot 6)
        ▼
Evaluation pipeline (01)        EvaluationPipeline · SnapshotComposer · MetadataRecorder ·
        │                       EmissionGuard (non-negative, after step 8)
        ▼
Frozen inputs (external SoRs)   commitmentPools[] snapshot (Contracts) · reservationMatch (ctx;
                                OSS/Contracts entitlement) · reservedRate/flavor (pinned catalog
                                row, pricing) · negotiated RI rate (step-5 overlay)
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | The step-6 evaluator and its intra-step order; obligation assembly | Rust module in the `rating` gear (rating-core crate), registered into the Foundation pipeline |
| Domain | Pool/reservation snapshot shapes, effect and obligation value objects (§3.1) | Rust; GTS + Rust domain structs |
| Infrastructure | **None** — no store, no cache; all inputs arrive frozen in the `EvaluationContext` | n/a |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Composition, not a model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-composition-cmt`

Commitment and reservation are commercial **compositions** over the steps-2–5 output (T-D-05): they
partition the evaluation unit and price the partitions at rates resolved upstream — no new `modelKind`,
no band math beyond the normative tier-counter exclusion (§4.2).

#### Frozen balances, thin evaluation

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-frozen-balances-cmt`

Every balance, draw order, rollover policy, flavor, and reserved quantity arrives frozen in the
context; Rating never reads, locks, or writes a live Contracts balance. One frozen tuple ⇒ a
byte-identical drawdown on replay ([`01-foundation.md`](./01-foundation.md) §4.2).

#### Surface, never post

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-surface-not-post-cmt`

Step 6 produces *effects and obligations*, not postings: the `TrueUpObligation` is a structured
value Billing executes ([`../PRD.md`](../PRD.md) §6.2, §9.2); the drawdown effect is outcome
lineage, not a balance mutation.

### 2.2 Constraints

#### Fixed slot, fixed intra-step order

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-fixed-slot-cmt`

Commitment is **always** evaluated at step 6 ([`../PRD.md`](../PRD.md) §6.6, §17.1 — "no
reordering"); within the step, reservation precedes pools and overage is the residual. Neither
order has a configuration surface.

#### Reversal math is slice 08's

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-reversal-boundary-cmt`

Correcting/negative usage **refills the drawn-down pool and emits compensating deltas without
driving a line negative** ([`../PRD.md`](../PRD.md) §6.2) — this slice owns the *shape* of the
drawdown effect that makes the refill computable; the reversal math itself runs in
[`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) under the Foundation
correction keys ([`01-foundation.md`](./01-foundation.md) §4.2).

#### Launch posture: single pool first

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-constraint-launch-posture-cmt`

The waterfall contract (ordered pools, snapshot carriage) is defined and frozen **now**; launch
exercises the single-pool default special case. Multi-pool drawdown and per-pool rollover
(burn-vs-carry) behavior are additive Follow-on over the same ordered `commitmentPools[]`
([`../PRD.md`](../PRD.md) §17.4) — the rollover *field* is frozen from day one.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-cmt`

- **`CommitmentPoolSnapshot`** — frozen per-pool input: pool id, contract ref, pool unit (quantity vs spend), **`poolType ∈ {prepaid_drawdown, committed_rate}`** (T-D-14 — decides zero-due vs in-arrears billability, §4.1), remaining balance **at `balanceVersion`** (T-D-10 — the cross-unit sequencing token the balance write-back cites, §4.1), declared draw order, rollover policy; the ordered set plus the reserved-vs-pool split rides the snapshot ([`../PRD.md`](../PRD.md) §6.6; segment enum: [`01-foundation.md`](./01-foundation.md) §4.3).
- **`ReservationMatch`** — optional context input ([`01-foundation.md`](./01-foundation.md) §3.1): match id, flavor (`consumption | capacity`), reserved-rate source (§4.3), `reservedQuantity`, coverage.
- **`CommitmentEffect`** — the recorded outcome lineage: per-pool draw amounts in declared order, in-commit vs overage quantity/spend split, applied rates.
- **`ReservationEffect`** — matched quantity priced at the reserved rate, the `capacityCharge` (capacity flavor), and the exclusions applied (pool drawdown, tier counter).
- **`TrueUpObligation`** — `(amount, period, contract ref)`; **shape owned by this slice**, emission envelope by the Foundation ([`01-foundation.md`](./01-foundation.md) §3.1); executed by Billing, never posted here.

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-commitment-evaluator-cmt`

- **`Step6Evaluator`** — the registered evaluator; enforces the intra-step order and the fail-closed input guards; composes the three parts below.
- **`ReservationMatcher`** — flavor dispatch; computes the matched/remainder split (consumption) or the allocation charge (capacity); signals the matched-quantity exclusion to the tier counter (§4.2).
- **`PoolWaterfall`** — ordered drawdown over `CommitmentPoolSnapshot[]`; produces the in-commit/overage split and per-pool draws; a pool absorbs at most its remaining balance — the waterfall itself can never produce a negative component.
- **`TrueUpAssembler`** — evaluates the contract's period-end true-up clause over the period aggregate into a `TrueUpObligation` per the §4.5 flavor formulas, invoked on the period-driven unit ([`01-foundation.md`](./01-foundation.md) §4.2, T-D-15); effects and applied rates are recorded through the Foundation `MetadataRecorder`, frozen pool/reservation identifiers ride the composed ref via the `SnapshotComposer` (the `commitmentReservation` segment — resolved 2026-07-11, T-D-09).

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-step6-evaluator-cmt`

The **step-evaluator contract** (in-process, registered into the pipeline): input = the
post-step-5 line state (resolved rates, bands, overlay lineage) + the frozen context
(`commitmentPools[]` snapshot, optional `reservationMatch`); output = `ReservationEffect` +
`CommitmentEffect` + obligations, applied to the line amount. Fail-closed problem values: torn or
missing pool snapshot fields; a `reservationMatch` with an unknown flavor or an unresolvable rate
source (§4.3); a reserved-vs-pool split absent when both constructs are present. No external API —
boundary contracts are owned by [`11-consumer-contracts.md`](./11-consumer-contracts.md).

### 3.4 Internal Dependencies

Upstream: [`01-foundation.md`](./01-foundation.md) (slot registration, obligation envelope,
`MetadataRecorder`, `EmissionGuard`, determinism keys);
[`03-metering-models.md`](./03-metering-models.md) (the model formula pricing the on-demand
remainder; its band math consumes this slice's matched-quantity exclusion — §4.2);
[`04-overlays-precedence.md`](./04-overlays-precedence.md) (steps 4–5 deliver the post-overlay
rates and the negotiated RI rate as the step-5 contract overlay). Downstream:
[`06-coupons.md`](./06-coupons.md) (post-commitment amount);
[`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) (reversal/refill);
[`09-period-plan-change.md`](./09-period-plan-change.md) (plan-change pool carry-vs-reset, period
wrappers).

### 3.5 External Dependencies

| Dependency | What arrives frozen | Contract |
|------------|--------------------|----------|
| Contracts & Agreements | `commitmentPools[]` snapshot (pool set, balances, draw order, rollover policy), true-up clause, reserved-vs-pool split; negotiated RI rates via the step-5 overlay | [`../PRD.md`](../PRD.md) §9.2, §13; [`11-consumer-contracts.md`](./11-consumer-contracts.md) |
| Pricing (Product Catalog) | `reservedRate` + `reservationFlavor` as attributes **on the single usage row**, frozen in the pinned snapshot; reservation joint fixture gates publish | [`../../../pricing/docs/design/10-advanced-primitives.md`](../../../pricing/docs/design/10-advanced-primitives.md) §3 |
| OSS / Contracts (entitlement) | `reservationMatch` entitlement lifecycle/inventory — a **hard precondition** for reservation pricing | [`../PRD.md`](../PRD.md) §17.3 |
| Billing | Executes `TrueUpObligation`; Rating posts nothing | [`../PRD.md`](../PRD.md) §9.2 |

### 3.6 Interactions and Sequences

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-flow-step6-line-cmt`

**Step 6 on one line** (within `cpt-cf-bss-rating-seq-evaluate-tariff`):

1. Guard: verify pool-snapshot integrity and, if `reservationMatch` is present, a known flavor and a resolvable reserved-rate source (§4.3); otherwise fail closed.
2. Capacity flavor: emit `capacityCharge = reservedRate × reservedQuantity × coveredGranules` (T-D-25 — the reservation's covered duration within the `AnchorPeriod`, in the row's billable-unit granules; §4.2) — independent of measured usage, never reduced by its absence, never drawing pools.
3. Consumption flavor: split the evaluation unit into matched (priced at the reserved rate) and on-demand remainder; the matched quantity is excluded from pool drawdown **and** from the on-demand tier counter; the remainder re-runs steps 3–5 as a unit (§4.2, T-D-13).
4. Waterfall: the remaining on-demand quantity/spend draws the ordered pools, each up to its remaining frozen balance; in-commit billability follows each pool's frozen `poolType` (§4.1, T-D-14); the residual beyond all pools prices as overage/on-demand — **selected by the frozen `overageRate` field (T-D-19)**: present ⇒ the flat frozen overage rate; absent ⇒ the post-step-5 banded on-demand rates of the remainder re-run.
5. Record: per-pool draws, the in-commit/overage split, and the reservation-match id go to metadata and the snapshot segment; the line then proceeds to step 7 and the `EmissionGuard` ([`01-foundation.md`](./01-foundation.md) §4.4).

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-trueup-period-cmt`

**Period true-up surfacing**:

1. When the contract defines period-end true-up, the **period-driven unit** at the `AnchorPeriod` boundary (synthesized by Rating's period tick — [`01-foundation.md`](./01-foundation.md) §4.2, T-D-15) evaluates the clause over the frozen period aggregate of the governing window unit, per the §4.5 flavor formulas.
2. Emit `TrueUpObligation(amount, period, contractRef)` through the Foundation envelope — surfaced for Billing execution, never an implicit posted charge ([`../PRD.md`](../PRD.md) §6.2, §12 AC 5).
3. Corrections against a period with a surfaced obligation flow as deltas under the correction key (slice 08); the obligation itself is never mutated in place.

### 3.7 Database Schemas and Tables

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-storage-none-cmt`

**None owned.** No table, no cache: pool balances and true-up clauses are Contracts'; the
reservation entitlement inventory is OSS/Contracts'; reserved-rate attributes are the pricing
gear's read model; the drawdown effect and obligations persist only inside the rated outcome owned
by Rating ([`01-foundation.md`](./01-foundation.md) §3.7).

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-cmt`

A stateless registered evaluator in the `rating` gear (rating-core crate) —
identical topology to the Foundation ([`01-foundation.md`](./01-foundation.md) §3.8). Per-partition
evaluation; no cross-partition pool coordination on the hot path (T-D-10, §4.1, covers cross-unit
balance sequencing).

## 4. Additional Context

### 4.1 Commitment-Pool Waterfall (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-waterfall-cmt`

- Drawdown runs over the **ordered** `commitmentPools[]` in declared order: each pool absorbs quantity/spend up to its remaining frozen balance before the next; the residual beyond all pools is overage/on-demand; a single pool is the default special case ([`../PRD.md`](../PRD.md) §6.6, §17.1 step 6).
- **Overage-rate selector (T-D-19, 2026-07-28 review #6):** the contract's frozen `commitmentReservation` segment MAY carry an **`overageRate`** per pool set. **Present** ⇒ the residual bills at that flat frozen rate (no band math on the residual — the §tier-counter asymmetry note applies); **absent** ⇒ the residual bills at the **banded on-demand rates**, placed over the **post-reservation remainder `Q`** — i.e. the `Q` that remains after the §4.2 reservation exclusion, banded from zero, and **not** reduced by pool draws (2026-07-28 review fix, flagged for veto: the earlier "full `Q`" was ambiguous on a line carrying **both** a reservation and pools — it read as including the matched reserved quantity, contradicting §4.2/§tier-counter asymmetry, which exclude it and re-band the remainder from zero). Amounts come from the post-step-5 stack. With no reservation on the line the remainder *is* the full `Q`, so the two readings coincide — which is why the ambiguity was invisible in the single-mechanism cases. The in-commit rate is analogous: `prepaid_drawdown` uses the frozen in-commit reference rate for the notional lineage, `committed_rate` bills the frozen in-commit rate — both always frozen. The earlier wording ("both values arrive frozen" unconditionally vs "where the contract defines distinct rates") described the two branches without naming the selector; the selector is the field's presence, frozen at contract publish — the pipeline consumes, never decides, the rate values ([`../PRD.md`](../PRD.md) §6.2).
- **Pool flavor and billability (adopted 2026-07-11, T-D-14)**: every pool carries a frozen `poolType ∈ {prepaid_drawdown, committed_rate}` (Contracts-authored, carried in the `commitmentReservation` segment). `prepaid_drawdown` — the pool was sold and billed **upfront at sale** (the commitment sale itself is a Contracts/Billing concern, out of rating-core scope); in-commit consumption emits a **zero-due line** whose notional value (`quantity × frozen in-commit reference rate`) rides the lineage for revenue reporting; no pool-driven true-up; the unused balance at period end follows the pool's rollover policy (burn vs carry) — never a clawback line. `committed_rate` — in-commit consumption **bills in arrears** at the frozen in-commit rate; the period-end shortfall true-up follows the §4.5 formulas. Overage residual bills per the T-D-19 selector in both flavors (frozen `overageRate` if present, else banded on-demand).
- Step 6 precedes FX (step 8): spend pools draw in **price currency**; quantity pools draw the step-3 normalized measure of the evaluation unit. A spend pool whose denomination differs from the line's price currency **fails closed** — no FX exists inside step 6 (conversion is step 8's); denomination coverage is a Contracts-side authoring obligation asserted at contract publish.
- **Tier-counter asymmetry (money-affecting)**: the in-commit quantity is **not** excluded from the tier counter `Q` — a commitment pool is a settlement instrument over the usage, not a capacity carve-out; band placement sees the full usage quantity. Only the **reservation** exclusion (§4.2) re-bands a remainder from zero. Where the frozen `overageRate` is present (T-D-19), that flat rate prices the residual without band math; absent, the residual is banded over the **post-reservation remainder `Q`** (reservation excluded per §4.2, pool draws **not** deducted — the two rules compose in that order, 2026-07-28 review fix). The joint fixture MUST assert this pool-vs-reservation asymmetry, **both selector branches**, the **combined reservation + pool + overage** case where the two exclusion rules meet, and the **reservation + sub-window slice** scenario (T-D-23 — `remainderOffsetQ` continuity across the cut; slice 03 §4.3).
- **Snapshot carriage (resolved 2026-07-11, T-D-09)**: the frozen pool set, per-pool balances, draw order, rollover policy, the optional **`overageRate`** (the T-D-19 residual-pricing selector — money-affecting, frozen at contract publish), and reserved-vs-pool split MUST be carried in `pricingSnapshotRef` ([`../PRD.md`](../PRD.md) §6.6) — they form the **eighth named segment `commitmentReservation`** (writer: Rating @ eval; no new writer, no pricing-side change), enumerated identically in [`01-foundation.md`](./01-foundation.md) §4.3, [`11-consumer-contracts.md`](./11-consumer-contracts.md) §4.7, and [`../SEAMS.md`](../SEAMS.md) S1 (2026-07-31 review, #5: all three "identical" lists — and the T-D-09 row — had omitted `overageRate`, so a composer built from any of them dropped the selector).
- **Balance write-back and sequencing (decided 2026-07-11, T-D-10)**: Contracts owns pool balances and **serializes** them. Rating, as the rated-outcome persister, publishes each outcome's **`CommitmentBalanceEffect`** (per-pool signed draw/refill deltas) to Contracts, idempotent on the emitting outcome's evaluation/correction key — **and, per T-D-27 (2026-08-01, joint with Contracts — flagged for veto), an effect is published for every outcome that *observed* a pool, not only those that drew or refilled one: a unit rated pure overage against an exhausted pool publishes a zero-draw effect carrying an overage marker.** This effect stream is Contracts' only source of Rating unit identity, and T-D-10 obliges Contracts to re-resolve every later unit that "drew, **or was rated overage against**", the pool — without the zero-draw effect those overage units were unnameable, so a retroactive refill cascaded to the drawers and never to the units that kept their overage price (2026-07-31 review, #7); Contracts applies effects per pool in received order, bumps the monotonic per-pool **`balanceVersion`**, and freezes `(balance, balanceVersion)` for subsequent context assembly — the `balanceVersion` in the `commitmentReservation` segment is exactly the frozen balance a unit observed, so cross-unit sequencing is `balanceVersion` order while the within-unit waterfall stays fully deterministic. A balance-affecting correction **cascades**: Contracts MUST emit re-resolution triggers for every later-`balanceVersion` unit that drew, or was rated overage against, the affected pool in the balance period — Rating routes each through `reresolve`, delta-only, each unit under its own pin ([`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) §4.4). The event schema is a Contracts/Rating cross-PRD obligation to mirror in their designs; Rating's own obligation still ends at recording the per-pool effects. **Over-draw detection (T-D-27, same joint obligation)**: with per-unit frozen balances and asynchronous write-back, two units sharing a pool at one frozen `balanceVersion` can each absorb the full remainder — the steady state under concurrent load, not a rare interleaving — while the §4.5 true-up formula is shortfall-only (`max(0, …)`) and every stated repair route is scoped to later-`balanceVersion` units, so nothing rating-side can see it. Contracts, the balance serializer, holds the only detecting position: on write-back it MUST detect aggregate applied draws at one `balanceVersion` exceeding that balance and emit the same T-D-10 re-resolution triggers for the over-drawn units (delta-only, each under its own pin). Rating's side is unchanged: publish effects (full observation coverage per this rule), honour triggers (2026-07-31 review, #8).

### 4.2 Reservation Flavors and Pool Precedence (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-reservation-flavors-cmt`

- **Consumption flavor**: the matched portion of measured usage prices at the reserved rate; the remainder prices at the on-demand rates resolved in steps 2–5. The reserved portion is **excluded from `commitmentPools[]` drawdown** — reservation precedes pools ([`../PRD.md`](../PRD.md) §6.6, §17.1 step 6).
- **Tier-counter exclusion** (adopted, money-affecting): the matched/allocated reserved quantity is excluded from the on-demand tier counter `Q` — only the on-demand remainder enters the row's bands, banded **from zero** (150K used against a 100K reservation ⇒ 100K at `reservedRate`, the remainder's `Q` starts at 0) — "from zero" = the remainder axis starts at the **window origin** with reserved quantity excluded; under sub-window slices the axis stays window-cumulative via `remainderOffsetQ`, never a per-slice reset (**T-D-23**, slice [`03`](./03-metering-models.md) §4.3). Frozen semantics per pricing `inst-rv-tier-q`; the reservation joint fixture MUST include a tiered-remainder scenario ([`../../../pricing/docs/design/10-advanced-primitives.md`](../../../pricing/docs/design/10-advanced-primitives.md) §3). **Recompute contract (T-D-13)**: on a consumption split, **steps 3–5 re-run as a unit over the on-demand remainder** — the slice-03 band math re-bands the remainder from zero, then the same frozen overlay stack (step 4) and contract overlay (step 5) re-apply to the re-banded amount (identical survivor set, order, and adjustment values; only the base amount changes — [`04-overlays-precedence.md`](./04-overlays-precedence.md) §4.2). The reserved-rate portion is **not** re-overlaid (§4.3). Compiled, not configurable; lineage records the superseded full-`Q` pass and the authoritative remainder pass.
- **Capacity flavor**: emit `capacityCharge = reservedRate × reservedQuantity × coveredGranules` regardless of measured usage — zero usage still bills the allocation; the charge is never reduced by absent usage and never draws `commitmentPools[]`. **Accrual basis (T-D-25, 2026-08-01 — flagged for veto)**: `reservedRate` is denominated in the usage row's billable unit — **level unit × granule duration** for level-billed rows (slice 03 §4.3) — so `reservedRate × reservedQuantity` is money **per granule**, not a period charge; the once-per-period emission multiplies by **`coveredGranules`** = the reservation's covered duration within the `AnchorPeriod` expressed in those granules, summed per sub-interval when the coverage or `reservedQuantity` changed mid-period (`Σ reservedRate × reservedQuantity_i × duration_i`) — a disk allocated on day 20, or resized mid-period, bills exactly its covered sub-intervals (before T-D-25 the formula carried no time factor at all, so proration was undefined: recurring-only proration (09 §4.2) never covered it and usage is never prorated); `reservedQuantity`, rate, and flavor are frozen in `pricingSnapshotRef` ([`../PRD.md`](../PRD.md) §6.6, §12 AC 20). The `capacityCharge` is emitted as its **own line** — a **period-driven unit** keyed `(subscription, priceId, chargeKind, lineKey, AnchorPeriod)` and serialized on `(subscription, AnchorPeriod)` per slice [`14`](./14-unit-synthesis-period-tick.md) §4.1 (capacity bills even on a zero-usage period, where no usage unit exists to key on); the selected usage row's identity rides the line as **lineage attachment** (row/price id + flavor + match id), never as the serialization/dedup key (envelope shape: [`11-consumer-contracts.md`](./11-consumer-contracts.md) §4.1).
- No `reservationMatch` ⇒ evaluation prices as pure usage; the reservation-match identifier, when present, is recorded in metadata **and** `pricingSnapshotRef` (the `commitmentReservation` segment — resolved 2026-07-11, T-D-09; the three "§4.1 open" pointers this slice had kept after the resolution were fixed by the 2026-07-31 review, #44).
- The entitlement source feeding `reservationMatch` (OSS / Contracts) is a **hard precondition** tracked in [`../PRD.md`](../PRD.md) §17.3; Rating consumes the match from the frozen context and never resolves entitlement.

### 4.3 Reserved-Rate Two-Source Rule (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-reserved-rate-sourcing-cmt`

Seam **M9** ([`../SEAMS.md`](../SEAMS.md)), adopted under **T-D-08**:

- **Self-service** reserved rates come from the **pinned catalog snapshot** — the `reservedRate`/`reservationFlavor` attributes on the selected usage row ([`../../../pricing/docs/design/10-advanced-primitives.md`](../../../pricing/docs/design/10-advanced-primitives.md) §3, [`../PRD.md`](../PRD.md) §6.6).
- **Negotiated RI-style** rates come from **Contracts**, arriving as the **step-5 contract overlay** — never as a catalog row.
- The rate step 6 prices with is the **post-step-5 effective value**: the snapshot attribute unless a negotiated contract term overlaid it at step 5 (contract outranks catalog base — [`../PRD.md`](../PRD.md) §17.1 step 5). The outranking happens in the overlay layer; step 6 makes no source choice of its own.
- A present `reservationMatch` whose rate resolves from **neither** source fails closed — the evaluator never guesses a reserved rate.

### 4.4 Commitment Pool vs Prepaid Credit Grant (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-pool-vs-grant-cmt`

Seam **M8** ([`../SEAMS.md`](../SEAMS.md)) — two constructs, one colliding word:

| | Rating **commitment pool** | Pricing **prepaid credit grant** |
|---|---|---|
| Construct | `commitmentPools[]`, ordered waterfall at step 6 | Plan-attached wallet primitive (`grantAmount`, `creditUnit`, `expiryPolicy`, `autoRechargeAllowed`) |
| Definition SoR | Contracts | Pricing gear ([`../../../pricing/docs/design/10-advanced-primitives.md`](../../../pricing/docs/design/10-advanced-primitives.md) §3) |
| Balance owner | Contracts | Billing/Rating (execution); GA-gated — grants definable, not sellable until balance execution exists |
| Drawdown evaluator | **Rating, this slice** | **Billing-executed** — never enters step 6 |

Normative: step 6 MUST NOT draw down a wallet-grant balance; the bare word "prepaid" MUST NOT be
used unqualified in Rating artifacts — always "commitment pool" (this slice) or "prepaid credit
grant" (the wallet, outside the §17.1 per-line order).

### 4.5 Obligations, Reversals, and Period Boundaries (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-obligations-boundary-cmt`

- **`TrueUpObligation`**: shape owned here `(amount, currency — explicit, the price currency (2026-07-31 review, #45: the sibling `PeriodFloorCapObligation` carries one; asymmetry removed), period, contract ref)`; emission envelope owned by the Foundation ([`01-foundation.md`](./01-foundation.md) §3.1); rides the Rating handoff ([`../PRD.md`](../PRD.md) §9.2) and is **executed by Billing** — surfaced, never posted.
- **True-up formulas (normative, T-D-14)**: `prepaid_drawdown` pools surface **no** pool-driven true-up (the pool was billed at sale; unused balance follows rollover). For `committed_rate` pools, the period-driven unit at the `AnchorPeriod` end evaluates per the contract's `commitmentBasis`: **quantity basis** — `TrueUpObligation.amount = max(0, committedQuantity − consumedInCommitQuantity) × frozen in-commit rate`; **spend basis** — `max(0, committedSpend − inCommitBilledAmount)`. Amounts leave at full intermediate precision in price currency (step-8 FX converts downstream); a zero-amount obligation is not emitted.
- **Guard order**: step-6 effects complete inside the steps-4–7 sequence, **before** the `EmissionGuard` non-negative clamp ([`01-foundation.md`](./01-foundation.md) §4.4); the waterfall itself cannot produce a negative component (a pool absorbs at most the remaining measure), so a negative line at the guard always originates elsewhere.
- **Reversals**: pool refill on correcting/negative usage, compensating deltas, and the never-negative rule are evaluated by [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) under the Foundation correction keys ([`01-foundation.md`](./01-foundation.md) §4.2); this slice guarantees the drawdown effect carries enough lineage (per-pool draws, order, rates) for a deterministic refill.
- **Plan-change boundary**: commitment-pool carry across a plan change **defaults to reset, and pricing publishes no pool flag** (pricing D-113, adopted as T-D-29 — the earlier "consumed frozen from the snapshot (default reset unless marked carry)" implied a snapshot field that does not exist: the only published plan-change continuity field is the singular `usageCounterOnPlanChange`, which governs the **tier counter**, slice [`09`](./09-period-plan-change.md) §4.1); pool carry stays Rating/Contracts-side pending the deferred committed-usage authoring, and the default is reset. This slice's evaluator runs unchanged on each side of the split.

## 5. Traceability

**Traces to**: `cpt-cf-bss-rating-fr-commitment-drawdown`, `cpt-cf-bss-rating-fr-reservation-consumption-flavor`, `cpt-cf-bss-rating-fr-capacity-charge`,
`cpt-cf-bss-rating-fr-committed-usage`

- **PRD**: §6.6 (`fr-commitment-drawdown`, `fr-reservation-consumption-flavor`, `fr-capacity-charge`), §6.2 (`fr-committed-usage`, hybrid attachment), §6.3 `fr-evaluation-order`, §17.1 step 6 + reserved-capacity note, §17.2 (plan-change pool carry-vs-reset), §17.3 (reservation entitlement precondition), §17.4 (multi-pool/rollover Follow-on), §12 ACs 5/19/20 (the vendored AC-numbering drift was fixed in the PRD 2026-07-28 — the rationales now cite AC 19/20).
- **Seams**: M8 (§4.4), M9 (§4.3); S1 segment-naming residue recorded in §4.1 — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-05 (composition, not modelKind), T-D-08 (reserved-rate two-source split), T-D-09 (`commitmentReservation` segment), T-D-10 (balance write-back + cascade), T-D-13 (steps-3–5 remainder re-run), T-D-14 (pool flavors + true-up formulas), T-D-15 (period-driven true-up unit) — [`../DECISIONS.md`](../DECISIONS.md).
- **ADR**: [`../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md`](../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md).
- **Pricing design set**: [`../../../pricing/docs/design/10-advanced-primitives.md`](../../../pricing/docs/design/10-advanced-primitives.md) (reserved attributes, tier-counter exclusion, prepaid credit grant, GA gate).
- **Step slices**: [`01-foundation.md`](./01-foundation.md) (pipeline, envelope, guards, keys); [`03-metering-models.md`](./03-metering-models.md) (remainder band math); [`04-overlays-precedence.md`](./04-overlays-precedence.md) (step-5 overlay, negotiated RI); [`06-coupons.md`](./06-coupons.md) (downstream step 7); [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) (reversals); [`09-period-plan-change.md`](./09-period-plan-change.md) (period/plan-change wrappers); [`11-consumer-contracts.md`](./11-consumer-contracts.md) (Contracts/Billing boundaries).
