Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Period & Plan-Change Obligations (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Pricing (Product Catalog), Subscriptions, Billing, Rating | Downstream: Billing, Rating | Owners: BSS Rating team -->

# DESIGN — Period & Plan-Change Obligations (Slice 9)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-design-period-plan-change`

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
  - [4.1 Adopted Proration and Anchor Enums (normative)](#41-adopted-proration-and-anchor-enums-normative)
  - [4.2 PeriodFloorCapObligation Envelope (normative)](#42-periodfloorcapobligation-envelope-normative)
  - [4.3 Sub-Window Split Semantics (normative)](#43-sub-window-split-semantics-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice owns the two **period-level** concerns that live *outside* the per-line §17.1 step
order — steps 1–9 have no slot for them ([`../PRD.md`](../PRD.md) §6.11, §17.1 period-level
note); together with slice 08 it "wraps the per-line flow"
([`01-foundation.md`](./01-foundation.md) §3.4). Concern one is the
**`PeriodFloorCapObligation`**: Rating resolves the floor/cap amount, currency, comparison
basis, and attachment scope from the pinned snapshot and *surfaces* a structured obligation;
**Billing executes** `max(total, floor)` / `min(total, cap)` during period aggregation, and
Billing rounds — Rating never applies either ([`01-foundation.md`](./01-foundation.md) §4.4).
Concern two is **sub-window proration**: when a `PriceWindow` activates mid-period, a plan
changes at `changeEffectiveAt`, or a **phase converts mid-period** (the third boundary kind —
§4.3), the invoice period splits into half-open UTC slices, each rated
as an ordinary per-line run over its *own* pinned snapshot and plan revision, with the
recurring component apportioned per the snapshot-frozen `prorationBasis`.

The slice introduces **no policy of its own**: the `prorationBasis` and `billingAnchorPolicy`
enums are the pricing gear's, adopted verbatim under a live CI gate (seams P1/P2,
[`../SEAMS.md`](../SEAMS.md)); the plan-change mode is Subscriptions' — Rating consumes
`(changeEffectiveAt, changeMode)` and **never decides the change**. What the slice owns is the
deterministic *math*: the anchor-boundary calendar (D-20 no-drift clamp), the split geometry,
the proration fractions, and the obligation shape.

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-fr-period-floor-cap-obligation` | The `PeriodObligationComposer` resolves amount, currency, comparison basis, and attachment scope from the pinned snapshot and emits a `PeriodFloorCapObligation` on the outcome envelope (§4.2); Billing executes and rounds; the per-line non-negative guard has already run — a floor never masks a negative line. |
| `cpt-cf-bss-rating-fr-mid-cycle-proration` | The `ProrationSplitter` cuts the invoice period at the activating window's `effectiveFrom` into `SubWindowSlice`s, each with its own snapshot, each emitted at full precision (§4.3); prorated recurring components use the frozen `prorationBasis`; Billing aggregates and rounds (AC 7). |
| `cpt-cf-bss-rating-fr-plan-change-proration` | planA rates over `[periodStart, changeEffectiveAt)`, planB over `[changeEffectiveAt, periodEnd)` — half-open, UTC, each against its own revision and snapshot (§4.3); tier-`Q`/pool carry-vs-reset follows snapshot-frozen flags; corrections to already-rated portions leave as deltas under the slice-08 correction key; `(changeEffectiveAt, changeMode)` is consumed, never decided. |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-throughput-latency` | `ProrationSplitter` | A split multiplies per-line runs only by the slice count, bounded by the window activations + plan changes inside one period (normally 0–1); each slice is an ordinary pipeline run over a pre-pinned snapshot — no new I/O | Load test with split fixtures |
| `cpt-cf-bss-rating-nfr-horizontal-scale` | Partitioning contract | Splits and period obligations stay inside the `(subscription, meter, dimensionKey, window)` partition; anchor math is context-local; zero cross-partition locks | Design + load test |
| `cpt-cf-bss-rating-nfr-resilience` | Fail-closed guards | Missing `periodState` fails closed (01 §2.1); a recurring row cannot lack `prorationBasis`/`billingAnchorPolicy` (pricing publish rejects it), but an absent frozen value still fails closed, never defaults | Chaos/retry test + conformance fixtures |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-bss-rating-adr-scope-key-adoption` | Every sub-window slice re-selects its base row on the adopted 8-axis key — a split never bypasses full-key selection. |
| `cpt-cf-bss-pricing-adr-pricewindow-consolidation` (adopted) | The pricing gear owns the `PriceWindow` store and activation; the mid-cycle split boundary is the pinned window's `effectiveFrom`, never a Rating-scheduled event. |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-ppc`

```text
Period phases (this slice)     AnchorCalendar · ProrationSplitter · PeriodObligationComposer
        │  (compose per-line runs; surface obligations)
        ▼
Per-line pipeline (01, 02–07)  one ordinary evaluate() per SubWindowSlice
        │
        ▼
Frozen inputs (external SoRs)  prorationBasis + billingAnchorPolicy + carry-vs-reset flags
                               (pricing snapshot) · (changeEffectiveAt, changeMode)
                               (Subscriptions) · periodState (Billing) · windowed Q (Rating)
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | Period wrappers around the per-line flow: split composition, obligation emission | Rust modules in the `rating` gear (rating-core crate), per 01 §3.8 |
| Domain | Anchor/D-20 calendar math, proration fractions, split geometry, obligation shape | Rust; GTS + Rust domain structs |
| Infrastructure | **None owned** — obligations and slice outcomes ride the Rating handoff; execution state is Billing's | Rating persistence; Billing period aggregation |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Surface, never execute

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-surface-not-execute-ppc`

Period-level floor/cap is emitted as a structured obligation and *executed by Billing* during
period aggregation. Rating never applies `max`/`min` over a period total, never rounds, and
never posts — obligations are surfaced, corrections to them flow as deltas (§4.2).

#### Split, never blend

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-split-never-blend-ppc`

A period containing a boundary is rated as discrete half-open sub-windows, each over its own
pinned snapshot and revision at full precision. There is no rate averaging, no blended price,
and no cross-slice reuse of a stale snapshot; Billing sums the slices (§4.3).

#### Consume the change, never decide it

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-consume-change-ppc`

`(changeEffectiveAt, changeMode)` arrives from Subscriptions, which owns WHEN a plan change
takes effect and the up/down asymmetry policy ([`../PRD.md`](../PRD.md) §17.2). The mode's only
evaluation-side effect is where the split boundary falls; Rating holds no change policy.

### 2.2 Constraints

#### No rounding, no period min/max in rating-core

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-no-round-no-execute-ppc`

rating-core MUST NOT apply period-level min/max at line aggregation and MUST NOT round; amounts leave
at full intermediate precision with the rounding-policy id recorded
([`../PRD.md`](../PRD.md) §6.11; 01 §4.4). The invoice-period total is Billing's sum of
sub-window amounts followed by Billing's rounding policy (AC 7).

#### Adopted enums, verbatim, CI-gated

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-enum-verbatim-ppc`

`prorationBasis` (including `none`) and `billingAnchorPolicy` are adopted verbatim from the
pricing gear and guarded by the CI gate `pricing.contracts.enum_drift` (Critical) — any local
extension or omission is a build-time block (seams P1/P2; T-D-07).

#### UTC half-open boundaries only

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-utc-half-open-ppc`

All period boundaries, split points, and anchor math are UTC with half-open `[from, to)`
intervals (01 §2.2 `constraint-utc-money-fnd`; D-20 anchor math is UTC by definition). A
boundary instant belongs to exactly one slice.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-ppc`

- **`PeriodFloorCapObligation`** — the value object this slice shapes (emission envelope owned by the Foundation, 01 §3.1): floor/cap amount, explicit currency, comparison basis, attachment scope (`usage` default, `recurring+usage` if plan-level — snapshot-frozen), period reference, contract/plan reference (§4.2).
- **`AnchorPeriod`** — the `[periodStart, periodEnd)` UTC boundaries implied by the frozen `billingAnchorPolicy` under the D-20 no-drift clamp; the authority for `invoice_period` boundaries and plan-change split points (seam P2).
- **`SubWindowSlice`** — one half-open `[from, to)` UTC slice of an `AnchorPeriod` bound to its own pinned snapshot and plan revision; the unit the per-line pipeline runs over during a split.
- **`ProrationFraction`** — the deterministic apportionment of a recurring amount to a slice per the frozen `prorationBasis` value (§4.1, §4.3); full-precision, never rounded here.
- **Frozen inputs** — `prorationBasis`, `billingAnchorPolicy`, **`usageCounterOnPlanChange`** — the **singular** tier-`Q` carry-vs-reset flag (pricing D-113, adopted as T-D-29: **absence = `reset`, never a rating failure**; pricing publishes **no pool flag** — pool carry defaults reset, slice [`05`](./05-commitments-reservations.md) §4.5; the earlier plural "flags … all fail-closed when absent" described fields pricing never published) — all from the pinned pricing snapshot; `(changeEffectiveAt, changeMode)` (Subscriptions); `periodState` (Billing). None owned; the Subscriptions/Billing inputs fail closed when absent, the pricing flag defaults per D-113.

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-period-plan-change-ppc`

- **`AnchorCalendar`** — computes `AnchorPeriod` boundaries and locates split points from the frozen `billingAnchorPolicy`, applying the D-20 clamp (day beyond month length → last day of month, anchor day preserved per period: 31→28→31, no drift); pure UTC calendar math, no I/O.
- **`ProrationSplitter`** — cuts the enclosing `AnchorPeriod` at each boundary (window `effectiveFrom`, `changeEffectiveAt`, phase-conversion instants) into ordered `SubWindowSlice`s; assembles one evaluation context per slice; applies `ProrationFraction` to recurring components; routes the boundary-continuity inputs to the tier counter (slice 03) and commitment pools (slice 05): mandatory carry (`bandOffsetQ` continuity) at window-activation and phase-conversion boundaries, the snapshot-frozen `usageCounterOnPlanChange` at a plan-change boundary — routed from the **target** plan's frozen snapshot (T-D-29) (§4.3, T-D-12).
- **`PeriodObligationComposer`** — resolves the floor/cap attachment from the pinned snapshot and builds the `PeriodFloorCapObligation`; hands it to the Foundation's emission envelope; records it in evaluation metadata.

### 3.3 API Contracts

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-interface-period-obligation-ppc`

The **obligation ride-along**: `PeriodFloorCapObligation` travels on the Rating handoff next to
the resolved outcome ([`../PRD.md`](../PRD.md) §9.2 Rating handoff / Billing `periodState`
contract). It is advisory-to-execute: Billing executes it at period aggregation; Rating holds
no execution state and receives no execution result.

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-interface-split-evaluation-ppc`

The **split contract** (conceptual, in-process): `split(AnchorPeriod, boundaries) → ordered
SubWindowSlice contexts`, each fed through the Foundation `evaluate()` unchanged. Replay of the
same frozen tuple yields byte-identical slices and amounts (01 §4.2); corrections to an
already-rated slice re-enter via `reresolve` under the correction key, never through a fresh
split of live state.

External boundary contracts (Billing, Subscriptions, pricing read model) are owned by
[`11-consumer-contracts.md`](./11-consumer-contracts.md).

### 3.4 Internal Dependencies

Depends on [`01-foundation.md`](./01-foundation.md) for the pipeline, guards, determinism keys,
and the obligation emission envelope. Each `SubWindowSlice` is a full 02–07 per-line run;
slices 03/05 consume the carry-vs-reset flags this slice routes at a plan-change boundary;
slice 07's FX policy converts a price-currency floor/cap for billing-currency comparison.
Delta/correction traffic for period obligations and already-rated slice portions flows under
[`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) keys — this slice never
mints its own correction path.

### 3.5 External Dependencies

| Dependency | What arrives frozen | Contract |
|------------|--------------------|----------|
| Pricing (Product Catalog) | `prorationBasis` (incl. `none`), `billingAnchorPolicy` + D-20 clamp, floor/cap attachment, `usageCounterOnPlanChange` (the singular tier-`Q` continuity flag — T-D-29 / pricing D-113 `inst-pc-counter-carry`; absence = reset; no pool flag exists) — all in the pinned snapshot; window `effectiveFrom` boundaries | [`../../../pricing/docs/design/06-consumer-contracts.md`](../../../pricing/docs/design/06-consumer-contracts.md); SEAMS P1/P2/P3 |
| Subscriptions | `(changeEffectiveAt, changeMode)`; plan revision linkage | [`../PRD.md`](../PRD.md) §9.2 Subscriptions input |
| Billing | `periodState` (open / closed_posted); executes floor/cap + rounding over the period aggregate | [`../PRD.md`](../PRD.md) §9.2 Billing contract |
| Rating pipeline (intra-gear — slices 13/15) | windowed `Q` per sub-window (single-writer, M7 key); persists outcomes, obligations, deltas | [`../PRD.md`](../PRD.md) §9.2 handoff; slices 13/15 |

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-sub-window-split-ppc`

**Sub-window split** (window activation or plan change):

1. A boundary falls inside the enclosing `AnchorPeriod`: an activating window's `effectiveFrom` (from the pinned read model), `changeEffectiveAt` (from Subscriptions, with `changeMode`), or a phase-conversion instant (from the frozen Subscriptions phase timeline — §4.3).
2. `AnchorCalendar` fixes `[periodStart, periodEnd)` from the frozen `billingAnchorPolicy` (D-20 clamp, UTC).
3. `ProrationSplitter` cuts the period into half-open slices; each binds its own pinned snapshot — and, for a plan change, its own plan revision (planA left of the boundary, planB right).
4. Each slice runs the ordinary per-line pipeline (steps 1–9); recurring components carry the `ProrationFraction` for the frozen `prorationBasis`; a `none` row is recognized and never prorated (§4.1); usage components are not prorated — each slice rates the usage attributed to it (capacity-flavor charges are neither recurring-prorated nor usage: they accrue by **covered granules** — T-D-25, slice [`05`](./05-commitments-reservations.md) §4.2).
5. At a plan-change boundary, tier `Q` carries or resets per the **target** plan's frozen `usageCounterOnPlanChange` under the per-line unit-field match guard (T-D-29; evaluated in slice 03); commitment pools default reset — no pool flag is published (slice 05 §4.5).
6. Every slice emits at full precision; Billing computes the period total as the sum of slices and applies its rounding policy; corrections to an already-rated portion leave as deltas under the slice-08 correction key.

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-period-floor-cap-ppc`

**Period floor/cap obligation**:

1. Per-line outcomes for the period exist; the non-negative guard has already run per line (01 §4.4) — before any period-level phase.
2. `PeriodObligationComposer` resolves floor/cap amount, currency, comparison basis, and attachment scope from the pinned snapshot.
3. The `PeriodFloorCapObligation` is emitted on the outcome envelope and recorded in metadata; nothing is applied locally.
4. Billing executes `max(total, floor)` / `min(total, cap)` during period aggregation, then rounds.
5. Late usage into an open window re-runs the composition deterministically over the same frozen tuple; any change to the obligation surfaces as a delta under slice-08 keys — a posted obligation is never mutated.

### 3.7 Database Schemas and Tables

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-storage-none-ppc`

**None owned.** Consistent with 01 §3.7: obligations and slice outcomes are emitted values
persisted by Rating; whether and how a floor/cap was executed is Billing's state; period
identity and `periodState` are Billing's; anchor math is derived, never stored. Loss of any
local computation is repaired by deterministic replay of the frozen inputs.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-ppc`

No new deployable, job, or scheduler **in Rating**: the period phases are rating-core / pipeline module code
in the `rating` gear (01 §3.8); the **period tick** that synthesizes the period-driven
evaluation units (recurring lines, capacity charges, true-up surfacing) at `AnchorPeriod`
boundaries is **Rating's** ([`11-consumer-contracts.md`](./11-consumer-contracts.md) §4.1,
T-D-15). Splits and obligations evaluate inside the existing
`(subscription, meter, dimensionKey, window)` partitions — a split multiplies per-line runs by
its slice count but never crosses a partition; period-obligation composition is per
subscription/period and carries no cross-partition lock.

## 4. Additional Context

### 4.1 Adopted Proration and Anchor Enums (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-proration-enums-ppc`

- `prorationBasis ∈ {calendar_days_actual, calendar_days_30, by_second, whole_unit, none}` — the pricing gear's canonical enum, adopted **verbatim** (seam P1, T-D-07); the CI gate `pricing.contracts.enum_drift` (Critical) blocks drift at build time. Value glosses per the pricing glossary: `calendar_days_30` uses a fixed 30-day month with the day count capped at 30; `whole_unit` performs no sub-period proration; `none` means no proration at all — full-period charge, no partial credit.
- **`none` recognition**: pricing rejects `creditOnDowngrade = true` with `prorationBasis = none` at publish ([`../../../pricing/docs/design/06-consumer-contracts.md`](../../../pricing/docs/design/06-consumer-contracts.md)), so Rating never prorates a `none` row — but MUST recognize the value and skip proration; the conformance fixture asserts all five values.
- `billingAnchorPolicy ∈ {calendar_month, subscription_start, fixed_day(d)}` with the **D-20 no-drift clamp**: `fixed_day(d)` — and `subscription_start` under monthly-granular cycles — with a day beyond the month length anchors on the last day of the month, the anchor day preserved across periods (each period clamps independently: 31→28→31, no drift); all anchor math UTC. Frozen in the snapshot; this is the sole authority for `invoice_period` boundaries and plan-change split points (seam P2).
- **Joint fixtures**: the shared golden proration/anchor fixture (`calendar_days_30` capping; `fixed_day(31)` → February month-end UTC rollover) plus the mid-period PriceWindow-split boundary fixture asserting identical results on the catalog↔Rating side.
- **Open**: the `whole_unit` attribution rule on a split (which slice bears the un-prorated whole unit) is fixed by neither PRD; to be settled with the joint proration fixture before Design lock. **Interim posture while the open stands (2026-07-31 review, #35 — the T-D-22 pattern: fail closed rather than guess)**: a `whole_unit` recurring line crossing a split point **fails closed at evaluation** — the §fail-closed rule had covered only an *absent* frozen value, not a present-but-unattributable one. A **`none`** line on a split is *not* covered by the open and does not fail: `none` means full-period charge with no partial credit (§4.1), and the whole amount **bears on the slice covering the anchor period's start instant** (the opening slice — the same family as the T-D-26 boundary rules).

### 4.2 PeriodFloorCapObligation Envelope (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-floor-cap-envelope-ppc`

- Rating sets the floor/cap **amount, currency, comparison basis, and attachment scope** and emits the structured obligation; **Billing executes** `max(total, floor)` / `min(total, cap)` during period aggregation and rounds. rating-core MUST NOT apply the min/max at line aggregation or round ([`../PRD.md`](../PRD.md) §6.11, §17.2).
- **Attachment**: to the usage component by default, or recurring+usage if plan-level — frozen in `pricingSnapshotRef` (§17.2).
- **Currency**: set in price currency; a billing-currency comparison converts with the same FX policy and `fxTableVersion` as step 8 (slice 07); the currency is always explicit — no implicit default (§17.2). **Under per-window rate-lock FX** the period aggregate spans many per-line locks, so "the same version as step 8" names no single lock: the comparison converts at the **period close-time `fxTableVersion`** — the step-8 rule for period-level artifacts (slice 07 §4.2 makes the close-time version authoritative for invoice-period aggregates; 2026-07-31 review, #38); floor/cap *execution* stays Follow-on (§17.4), so the rule binds the emitted comparison basis only.
- **Ordering**: the per-line non-negative guard (01 §4.4) runs before any period-level phase; a floor MUST NOT mask a negative line.
- **Surfaced, never posted**: the obligation is an emitted value, not a posting; re-emission for a re-opened window is deterministic over the same frozen tuple, and changes flow as deltas under the slice-08 correction key `(unitKey[, slice], prior-rated-version, snapshot)` (01 §4.2).
- **Phasing**: the boundary and obligation shape are defined now; floor/cap *execution* is phased Follow-on on the Billing side ([`../PRD.md`](../PRD.md) §17.4) — the contract does not change when execution lands.
- **Open** ([`../PRD.md`](../PRD.md) §15): whether a contractual floor claws back coupon discount — default proposal: the floor compares the post-coupon total; carried here unresolved.

### 4.3 Sub-Window Split Semantics (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-split-semantics-ppc`

- **Split points** — three kinds: an activating window's `effectiveFrom` (mid-cycle activation), `changeEffectiveAt` (plan change), and a **phase-conversion instant** from the frozen Subscriptions phase timeline (slice 02 `PhaseContext`) — `phase_id` is a key axis, so a mid-period trial→intro→evergreen conversion changes the selected row, and a recurring component spanning the conversion prorates across the boundary exactly like a window activation. Coincident boundaries collapse into one cut (the boundary instant belongs to the right-hand slice; a collapsed cut involving a plan-change boundary consults the plan-change flag for counter continuity — the only configurable rule at the cut; slice [`03`](./03-metering-models.md) §4.3, 2026-07-31 review #37). Geometry is half-open UTC: the old window/planA covers `[periodStart, boundary)`, the new window/planB covers `[boundary, periodEnd)`; each slice binds its own plan revision and pinned snapshot and emits at full precision; Billing totals then rounds (AC 7, AC 17).
- **Recurring proration**: the slice fraction follows the frozen `prorationBasis` over `AnchorCalendar` denominators — `calendar_days_actual`: covered UTC days over actual days of the anchor period, where **the boundary day belongs to the slice covering that day's 00:00 UTC** (T-D-26, 2026-08-01 — the slice that *opened* the day, the same precedent as the straddling package block, slice 03 §4.3: split points are instants, so without the rule a mid-day cut left the boundary day countable in slice A, B, both or neither — 10/31 + 22/31 over-bills, 9/31 + 21/31 under-bills) and **a period's slice fractions MUST sum to exactly 1** (invariant, asserted by the joint fixture); `calendar_days_30`: the 30-day-month convention with day count capped at 30 — **the cap applies per period**: split fractions compute against the 30 denominator and clamp so the period total never exceeds 1 (a 20+11-day split bills 30/30, never 31/30 — 2026-07-31 review, #36; the split-scope leg stays on the joint golden-fixture list); `by_second`: covered seconds over period seconds; `whole_unit`/`none`: no fractional apportionment (§4.1 — split posture below). Fractions are computed at full intermediate precision and never rounded here.
- **Usage**: never prorated — usage attributes to a slice by event/window time; the windowed `Q` stays Rating-owned under the M7 key. When a split point falls **inside an open aggregation window**, each slice is its own evaluation unit under the band-offset continuity of [`03-metering-models.md`](./03-metering-models.md) §4.3 (T-D-12): window-activation/supersession and phase-conversion boundaries **always carry** the counter (pricing `inst-tb-window-continuity` — not configurable); only a *plan-change* boundary consults the snapshot-frozen **`usageCounterOnPlanChange`** — routed from the **target** plan's frozen snapshot (the plan whose bands consume the continued `Q` accepts the continuity liability); **absence = `reset`, never a rating failure** (pricing D-113 / T-D-29 — this supersedes the slice's earlier "fail-closed when absent" reading, named by pricing's register as the one being reconciled away); **`carry` is honoured per shared `(meter, dimensionKey)` line and only where the source and target rows match on the D-82/D-98/D-122 unit field set** (`model_kind`, `billingGranularity`, `aggregationFunction`, `aggregationGranularity`, `tierAggregationWindow`, `tierQualificationWindow`, `package_size`) **across both frozen snapshots** — a mismatched line resets (+ an operator signal), never a cross-denomination carry: a `per_hour` → `per_day` carry would apply an hours-denominated `Q` to day-denominated bands (`reset` ⇒ `bandOffsetQ = 0`, §17.2); commitment-pool carry stays reset — pricing publishes no pool flag (slice 05 §4.5).
- **Change mode**: `(changeEffectiveAt, changeMode)` is consumed from Subscriptions; the mode's only evaluation-side effect is the boundary position — WHEN and the up/down asymmetry policy are Subscriptions' (§17.2). Rating MUST NOT decide the change mode.
- **Seat-count changes are not a Rating split point**: a mid-period `subscription_seat_count` change for a `per_unit` row reaches Rating only as a Subscriptions-driven change boundary (Subscriptions owns runtime proration execution and WHEN); each slice rates with the seat count frozen in its context. The concrete transport (a `changeEffectiveAt`-shaped boundary vs Subscriptions-side proration) is an **open with Subscriptions** — recorded, default: the change-boundary reading.
- **Anchor change across a plan change**: the enclosing `AnchorPeriod` geometry is fixed once, at `periodStart`, by the anchor policy then in force (planA's); a plan change that alters `billingAnchorPolicy` takes effect from the **next** period boundary — asserted in the joint anchor fixture; period identity itself stays Billing/Subscriptions' — **and since T-D-33 it is consumed, not derived**: `AnchorPeriod` ≡ the subscriptions period fact's identity, this gear's calendar supplies geometry only, and the fixture asserts the two coincide (subscriptions executes the D-20 clamp on its emitter side — its SUB-D-27).
- **Cross-boundary guarantee**: pricing rejects in-place proration for a mid-cycle change crossing currency, region, or billing frequency (handled as cancel + new) — Rating never receives a cross-boundary in-place split ([`../../../pricing/docs/design/06-consumer-contracts.md`](../../../pricing/docs/design/06-consumer-contracts.md)).
- **Determinism**: split geometry, fractions, and obligations are pure functions of the frozen tuple (01 §4.2); replay yields byte-identical slices; corrections to already-rated portions are deltas via the Adjustment path — never a mutation.

## 5. Traceability

**Traces to**: `cpt-cf-bss-rating-fr-period-floor-cap-obligation`, `cpt-cf-bss-rating-fr-mid-cycle-proration`, `cpt-cf-bss-rating-fr-plan-change-proration`

- **PRD**: §6.11 (all three FRs), §17.1 period-level phase note, §17.2 (floor/cap + plan-change boundary contracts), §9.2 (Billing `periodState`/obligation, Subscriptions input, pricing read-model contracts), AC 7/17, §7.1 NFRs, §15 (floor-vs-coupon open), §17.4 (floor/cap execution phasing).
- **Seams**: P1 (`prorationBasis` incl. `none`, CI gate), P2 (`billingAnchorPolicy` + D-20 clamp) — [`../SEAMS.md`](../SEAMS.md), incl. the ownership matrix row "Proration/plan-change *math* — Rating evaluates; Subscriptions executes".
- **Decisions**: T-D-07 (enums verbatim + CI gate), T-D-03 (snapshot composition carries the frozen policy fields), T-D-04 (snapshot-only replay for corrections) — [`../DECISIONS.md`](../DECISIONS.md).
- **Pricing design set**: [`../../../pricing/docs/design/06-consumer-contracts.md`](../../../pricing/docs/design/06-consumer-contracts.md) (K1/K2 enum ownership, D-20, cross-boundary rejection, joint fixtures).
- **Slices**: [`01-foundation.md`](./01-foundation.md) (pipeline, guards, envelope, determinism keys), [`03-metering-models.md`](./03-metering-models.md)/[`05-commitments-reservations.md`](./05-commitments-reservations.md) (carry-vs-reset consumers), [`07-currency-fx.md`](./07-currency-fx.md) (floor/cap currency comparison), [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) (delta/correction boundary), [`11-consumer-contracts.md`](./11-consumer-contracts.md) (boundary contracts).
