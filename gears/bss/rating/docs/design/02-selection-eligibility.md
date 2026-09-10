Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Base Selection & Eligibility (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Pricing (Product Catalog), Subscriptions | Downstream: Rating | Owners: BSS Rating team -->

# DESIGN — Base Selection & Eligibility (Slice 2)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-design-selection-eligibility`

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
  - [4.1 Selection Algorithm (normative)](#41-selection-algorithm-normative)
  - [4.2 Eligibility Classes and Cohort Generations (normative)](#42-eligibility-classes-and-cohort-generations-normative)
  - [4.3 Phase Semantics (normative)](#43-phase-semantics-normative)
  - [4.4 Selection Failure Taxonomy (normative)](#44-selection-failure-taxonomy-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

Base Selection & Eligibility is the **steps 1–2 evaluator** registered into the Foundation
pipeline ([`01-foundation.md`](./01-foundation.md) §3.2): resolve the active plan **phase**
(`phase_id`) at `t`, then select **the single** `Price`/`PriceWindow` row on the pricing
**8-axis canonical scope key** — per emitted line's `chargeKind`. Everything downstream (model
math, overlays, commitments, coupons, FX) prices *the row this slice selected*; a selection
defect is therefore the worst class of mispricing, and the slice's whole posture is
**fail-closed over frozen inputs**: no silent fallback, no `activatedAt` heuristics, no
kind-name phase matching.

The slice owns the **selection policy**: candidate-set construction (including the D-15
phase-invariant usage fallback), the `priceEligibility` **class order**, **multi-generation
`cohort` selection by the subscription's pinned price id**, the single-match assertion, and the
selection failure taxonomy. It does **not** own the key definition (pricing gear SoR, adopted
verbatim — 01 §4.1), the key-materialization mechanism (`ScopeKeyAdapter`, Foundation), overlay
stacking (step 4, [`04-overlays-precedence.md`](./04-overlays-precedence.md)), or meter mapping
(step 3, [`03-metering-models.md`](./03-metering-models.md)). Inputs arrive frozen: the pinned
catalog read model (pricing), the phase/eligibility context (Subscriptions), and the pinned
price ids with their `cohort` in the `pricingSnapshotRef` pre-stamp
([`../PRD.md`](../PRD.md) §6.3, §6.5).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-fr-base-catalog-selection` | `SelectionEvaluator` selects on the full 8-axis key with `t in [effectiveFrom, effectiveTo)`; "at most one window matches" is asserted **only on the full key** — coexisting hybrid `chargeKind` rows and `cohort` generations are disambiguated by the key, never fail-closed (§4.1); no match on billable usage ⇒ fail-closed problem (§4.4). |
| `cpt-cf-bss-rating-fr-price-eligibility-grandfathering` | `EligibilityClassFilter` applies `existing_grandfathered > new_subscriptions_only > all_subscriptions`; `CohortGenerationSelector` picks the generation whose `cohort` equals the cohort of the subscription's **pinned price id** in `pricingSnapshotRef` — never `activatedAt` alone (§4.2). |
| `cpt-cf-bss-rating-fr-plan-phases` | `PhaseResolver` resolves the active `phase_id` (uuid, pricing D-19) at `t` from the frozen Subscriptions phase context; `CandidateSetBuilder` implements the D-15 phase-invariant usage fallback so the no-gap rule applies to the *resolved* set (§4.3). |
| `cpt-cf-bss-rating-fr-evaluation-order` (steps 1–2 slots) | The evaluator registers into the fixed step-1/step-2 slots of the Foundation pipeline; the order itself is owned by 01 — this slice contributes policy, not sequencing. |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-throughput-latency` (p95 ≤ 100ms lookup) | `CandidateSetBuilder` over the pinned read model | Selection is in-memory filtering over pinned read-model pages (Foundation resolved-window cache, 01 §3.7); no I/O, no catalog query inside the evaluator | Load test; targets provisional (NFR workshop, [`../PRD.md`](../PRD.md) §7.1) |
| `cpt-cf-bss-rating-nfr-horizontal-scale` | `SelectionEvaluator` | Stateless pure function per evaluation unit; nothing selection-side blocks per-partition parallelism | Design review |
| `cpt-cf-bss-rating-nfr-resilience` | Failure taxonomy §4.4 | Every unresolved selection is a typed fail-closed problem (no guess, no default row); retries replay to the identical outcome (01 §4.2) | Chaos/retry test + joint fixtures |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-bss-rating-adr-scope-key-adoption` | Adopt the pricing 8-axis key verbatim for selection + non-overlap; no Rating-local key (SEAMS K1–K5). |
| `cpt-cf-bss-pricing-adr-canonical-scope-key` (adopted) | The key definition — 8 additive axes; the pricing gear is its SoR. |
| `cpt-cf-bss-pricing-adr-grandfathering-cohort-axis` (adopted) | `cohort` = the cutover instant; N generations coexist; the generation is selected by the pinned price id's cohort. |
| `cpt-cf-bss-pricing-adr-pricewindow-consolidation` (adopted) | `PriceWindow*` events (all four, incl. `Cancelled`) are read-only resolution inputs; pricing owns the window store and state machine. |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-sel`

```text
SelectionEvaluator (this slice)   PhaseResolver · CandidateSetBuilder · EligibilityClassFilter ·
        │  (registers into §17.1 slots 1–2)      CohortGenerationSelector · SelectionGuard
        ▼
Foundation mechanisms (01)        ScopeKeyAdapter (key materialization) · EvaluationPipeline ·
                                  resolved-window cache · SnapshotComposer pre-stamp checks
        │
        ▼
Frozen inputs                     pinned catalog read model (pricing) · phase/eligibility ctx
                                  (Subscriptions) · pinned price ids + cohort (snapshot pre-stamp)
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | The steps 1–2 evaluator and its selection policy | Rust module in the `rating` gear (rating-core crate) |
| Domain | Candidate set, eligibility classes, cohort pin, selection outcome shapes | Rust; GTS + Rust domain structs |
| Infrastructure | **None owned** — reads pinned read-model pages via the Foundation cache | In-process (01 §3.7) |

## 2. Principles and Constraints

### 2.1 Design Principles

#### The full key or nothing

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-full-key-only-sel`

Uniqueness exists **only on the full 8-axis key**. Shorter tuples are non-unique *by pricing
design* (hybrid `chargeKind` rows; grandfathered generations + successor coexisting); treating
a short-tuple multi-match as an error is itself the defect (SEAMS K1/K3). The evaluator never
selects, asserts, or fails on a partial key.

#### Class order first, then the cohort pin

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-class-then-cohort-sel`

Eligibility is a two-stage filter: the class order picks the most specific eligible class;
within `existing_grandfathered` the **pinned price id's `cohort`** picks the generation. The
subscription's `activatedAt` bounds class membership but **never** selects a generation
(SEAMS K2).

#### Gaps are judged on the resolved set

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-resolved-set-no-gap-sel`

The no-gap rule applies **after** the D-15 phase-invariant usage fallback resolves the
candidate set: a phase covered only by a phase-invariant usage row is *not* a gap; a truly
uncovered billable phase *is* — and fails closed (SEAMS F1).

### 2.2 Constraints

#### No silent fallback

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-no-silent-fallback-sel`

If no eligible window matches on the full key, evaluation **fails for billable usage** — never
a default row, a nearest window, or a zero rate ([`../PRD.md`](../PRD.md) §6.3). The failure is
a typed problem (§4.4), surfaced with the materialized key for diagnosis.

#### `phase` is a `phase_id`

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-phase-id-only-sel`

The phase axis joins on a **uuid `phase_id`** (pricing D-19); kind names (trial / intro /
evergreen) are display-only and never match anything. Non-phased and one-time rows ride the
implicit **terminal `phase_id`** — the evaluator treats them as ordinary keyed rows (SEAMS K5).

#### Selection inputs are frozen

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-frozen-selection-inputs-sel`

The phase timeline, `activatedAt`, and the pinned price ids with their `cohort` all arrive in
the frozen `EvaluationContext` / snapshot pre-stamp; the evaluator performs **no Subscriptions
or catalog query** on the hot path (01 §2.1). Re-resolution replays the same frozen inputs
(SEAMS W2).

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-sel`

All value objects; the slice owns no entities — catalog rows are frozen inputs.

- **`PhaseContext`** — the frozen Subscriptions phase timeline for the subscription; yields the active `phase_id` at `t` (or a typed absence).
- **`SelectionKey`** — the materialized 8-axis tuple `(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)` produced via the Foundation `ScopeKeyAdapter`; the `priceOverlay` axis of the **base** row (overlay lists are step-4 material, slice 04).
- **`CandidateSet`** — the pinned-read-model rows admitted for `(t, phase_id ∪ phase-invariant, chargeKind)` before eligibility filtering; retains per-row provenance for diagnostics.
- **`EligibilityClass`** — `existing_grandfathered > new_subscriptions_only > all_subscriptions` (ordered; the order *is* the domain fact).
- **`CohortPin`** — the pinned price id and its `cohort` extracted from the `pricingSnapshotRef` pricing pre-stamp; absence while an `existing_grandfathered` candidate exists is a torn-pin failure (§4.4).
- **`SelectionOutcome`** — the selected row (stable `{skuId, planId, priceId}`), window identity, winning class, matched `cohort`, resolved `phase_id`, the phase-invariant-fallback flag, and the **FX-skip flag** (invoice currency = price currency ⇒ step 8 skipped) — recorded into evaluation metadata and consumed by steps 3+.

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-selection-evaluator-sel`

`SelectionEvaluator` — the registered steps 1–2 evaluator; policy lives here, mechanism in the
Foundation (`ScopeKeyAdapter` materializes the key; the pipeline sequences the slots):

- **`PhaseResolver`** — step 1: the active `phase_id` at `t` from `PhaseContext`; fails closed on an unresolvable phase.
- **`CandidateSetBuilder`** — admits rows effective at `t` for the resolved `phase_id` **plus** phase-invariant usage rows (D-15); phase-specific wins where both cover the same remaining key (§4.3).
- **`EligibilityClassFilter`** — applies the class order; evaluates `new_subscriptions_only` / `existing_grandfathered` membership from frozen `activatedAt` vs window `effectiveFrom` / cutover (§4.2).
- **`CohortGenerationSelector`** — within `existing_grandfathered`, keeps exactly the row whose `cohort` equals `CohortPin.cohort` (§4.2).
- **`SelectionGuard`** — asserts exactly one surviving row per full key per emitted line's `chargeKind`; >1 is a catalog non-overlap violation (a **pricing-side defect**, fail-closed and surfaced, never tie-broken); 0 on billable usage is `no_eligible_window` (§4.4).

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-select-base-row-sel`

The **selection contract** (internal; invoked only by the Foundation pipeline, never a public
surface): `select(EvaluationContext, pinned read model) → SelectionOutcome | SelectionProblem`.
Deterministic: same frozen context + same pinned pages ⇒ the identical outcome, on first
evaluation and on snapshot replay (SEAMS W2). Problems are the typed fail-closed values of §4.4
— this slice's contribution to the error taxonomy promised in 01 §3.3.

### 3.4 Internal Dependencies

Upstream: [`01-foundation.md`](./01-foundation.md) — pipeline slots, `ScopeKeyAdapter`,
snapshot pre-stamp integrity (a torn pre-stamp is rejected by the `SnapshotComposer` before
selection runs). Downstream: [`03-metering-models.md`](./03-metering-models.md) prices the
selected row; [`04-overlays-precedence.md`](./04-overlays-precedence.md) stacks overlays on it;
[`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) replays this same
evaluator over the pinned snapshot for corrections.

### 3.5 External Dependencies

| Dependency | What arrives frozen | Contract |
|------------|--------------------|----------|
| Pricing (Product Catalog) | pinned read-model rows on the 8-axis key (windows, eligibility, cohorts); `PriceWindowScheduled/Activated/Expired/Cancelled` + `CatalogVersionPublished` as **cache-invalidation** signals only | [`11-consumer-contracts.md`](./11-consumer-contracts.md); SEAMS C1, W1 |
| Subscriptions | phase timeline / active `phase_id` inputs, `activatedAt`, the subscription's pinned price id binding | PRD §9.2 Subscriptions input |
| Pricing pre-stamp (via snapshot) | resolved price ids **incl. `cohort`** | 01 §4.3; SEAMS S1 |

### 3.6 Interactions and Sequences

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-flow-select-base-row-sel`

**Select the base row** (steps 1–2 of `cpt-cf-bss-rating-seq-evaluate-tariff`):

1. `PhaseResolver`: active `phase_id` at `t` from the frozen `PhaseContext` (fail closed if unresolvable).
2. `CandidateSetBuilder`: rows effective at `t` for `phase_id` ∪ phase-invariant usage rows; phase-specific wins on collision (D-15).
3. `EligibilityClassFilter`: keep the most specific eligible class per the total order (§4.2).
4. `CohortGenerationSelector`: within `existing_grandfathered`, keep the row with `cohort = CohortPin.cohort`.
5. `SelectionGuard`: exactly one row per full key per emitted line's `chargeKind` — hybrid plans select one recurring **and** one usage row, each on its own key (SEAMS K3, owned by 01's key adoption; enforced here).
6. Emit `SelectionOutcome` (+ FX-skip flag when invoice currency = price currency); record class / cohort / `phase_id` / fallback flag into evaluation metadata; hand to step 3.

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-phase-fallback-sel`

**Phase-invariant usage fallback** (SEAMS F1): a usage line lands in a phase with no
phase-scoped usage row → the phase-invariant row is selected (not a gap, no failure); where a
phase-scoped usage row exists for the same remaining key, it wins for its phase. Recorded via
the fallback flag so fixtures can assert which path priced the line.

### 3.7 Database Schemas and Tables

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-storage-none-sel`

**None owned.** Selection reads pinned read-model pages through the Foundation's
non-authoritative resolved-window cache (01 §3.7); cache loss degrades latency, never
correctness or selection outcome.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-sel`

Nothing beyond the Foundation posture (01 §3.8): a module in the `rating` gear (rating-core crate);
stateless per evaluation unit; safe under per-partition parallelism.

## 4. Additional Context

### 4.1 Selection Algorithm (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-selection-algorithm-sel`

For evaluation at `t` (UTC) over a frozen context, per emitted line (`chargeKind`):

1. Resolve the active `phase_id` at `t` (step 1). Unresolvable ⇒ `missing_phase_context`.
2. Build the candidate set: rows with `t in [effectiveFrom, effectiveTo)` keyed to `phase_id`, **plus** phase-invariant usage rows; phase-specific wins on collision (§4.3).
3. Apply the eligibility class order; within `existing_grandfathered`, apply the cohort pin (§4.2).
4. Assert exactly one survivor **on the full 8-axis key**: `0` on billable usage ⇒ `no_eligible_window`; `>1` ⇒ `selection_ambiguity` (§4.4).
5. Emit `SelectionOutcome`; when invoice currency equals the row's price currency, set the FX-skip flag (advisory metadata — [`07-currency-fx.md`](./07-currency-fx.md) re-derives the skip authoritatively from `CurrencyRoles`).

The algorithm is a pure function of `(EvaluationContext, pinned read model)`; replay and
re-resolution execute it unchanged over the pinned snapshot (SEAMS W2).

### 4.2 Eligibility Classes and Cohort Generations (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-eligibility-cohort-sel`

- Class order (total, most-specific-wins): `existing_grandfathered > new_subscriptions_only > all_subscriptions` (pricing `design/07`, adopted verbatim — SEAMS K4).
- `new_subscriptions_only` **excludes** subscriptions with `activatedAt` before the window `effectiveFrom`; `existing_grandfathered` includes **only** subscriptions activated before cutover.
- **Multi-generation grandfathering** (pricing `ADR/0002`, SEAMS K2): N generations — distinct `cohort`s, each an active window — legitimately coexist on one key. The generation is the row whose `cohort` equals the cohort of the subscription's **pinned price id** in `pricingSnapshotRef`. `activatedAt` alone **never** selects a generation; no new store is needed — the pin already exists.
- Outside `existing_grandfathered` the `cohort` axis is always **`none`** — the pin never participates in selecting non-grandfathered rows.
- A present pin whose `cohort` matches **no live generation** while grandfathered candidates exist is `cohort_pin_unmatched` (§4.4) — never a neighbouring-generation pick, never a silent fall-through; when no grandfathered candidate is live at `t` (all generation windows expired), the class filter naturally proceeds to the remaining classes.
- Guarded by the joint fixture set: multi-generation coexistence + hybrid selection are the cohort/phase-fallback anchor fixtures shared with the pricing gear (01 §4.1); the fixture set MUST include the unmatched-pin scenario.

### 4.3 Phase Semantics (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-phase-semantics-sel`

- `phase` is always a **uuid `phase_id`** (pricing D-19); trial / intro / evergreen are display names and never join the axis (SEAMS K5).
- Non-phased and one-time rows ride the implicit **terminal `phase_id`** and select like any other row — for a one-time row, selection serves coverage/preview/quote only: no evaluation unit is ever synthesized for it (T-D-18; Subscriptions/Billing bill it at sale).
- Distinct phases MAY have schedules coexisting at the same `t` — not an overlap: `phase` is a key axis ([`../PRD.md`](../PRD.md) §6.5).
- **Usage rows are phase-invariant by default** (pricing D-15): one usage row spans all phases; an explicit phase-scoped usage row wins for its phase. The no-gap rule applies to the **resolved** set — a phase covered only by the phase-invariant row is not a gap (SEAMS F1). Mechanically the phase-invariant usage row **is** the terminal-`phase_id` row (the D-15/D-19 reconciliation) — no separate catalog flag exists.
- **Subscription-less (quote-time) contexts** — e.g. bundle `sum_of_parts` summing at quote — carry no Subscriptions phase/eligibility inputs and default to: the plan's terminal `phase_id`, `priceEligibility = all_subscriptions`, `cohort = none`, no proration ([`11-consumer-contracts.md`](./11-consumer-contracts.md) §4.3).

### 4.4 Selection Failure Taxonomy (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-failure-taxonomy-sel`

All fail-closed problem values (01 §3.3); each carries the materialized `SelectionKey` and the
candidate-set provenance for diagnosis:

| Problem | Condition | Semantics |
|---------|-----------|-----------|
| `no_eligible_window` | zero survivors on the full key for **billable** usage | evaluation fails; no fallback, no zero-rate |
| `selection_ambiguity` | more than one survivor on the full key | catalog **non-overlap violation** — a pricing-side publish defect; fail closed and surface to the pricing gear; never tie-broken locally |
| `missing_phase_context` | no active `phase_id` resolvable at `t` | fail closed; the frozen Subscriptions input is incomplete |
| `torn_cohort_pin` | `existing_grandfathered` candidates present but the snapshot pre-stamp lacks the pinned price id / `cohort` | fail closed; snapshot integrity failure (01 §4.3 — the `SnapshotComposer` never fabricates a segment) |
| `cohort_pin_unmatched` | the pin is **present** but its `cohort` matches no candidate generation while `existing_grandfathered` candidates exist | fail closed; a lifecycle/re-bind inconsistency — **never** a neighbouring-generation pick and never a silent fall-through to `all_subscriptions` (fall-through is legal only when no grandfathered candidate is live at `t` at all — expired windows simply leave the candidate set) |

## 5. Traceability

**Traces to**: `cpt-cf-bss-rating-fr-base-catalog-selection`, `cpt-cf-bss-rating-fr-price-eligibility-grandfathering`, `cpt-cf-bss-rating-fr-plan-phases`

- **PRD**: §6.3 `fr-base-catalog-selection` + steps 1–2 slots of `fr-evaluation-order`; §6.5 `fr-price-eligibility-grandfathering`, `fr-plan-phases`; §17.1 steps 1–2 (normative order).
- **Seams**: K1 (full 8-axis key), K2 (cohort by pinned price id), K4 (class order), K5 (`phase_id` typing), F1 (phase-invariant fallback) — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-01, T-D-08 (F1 part) — [`../DECISIONS.md`](../DECISIONS.md).
- **ADR**: [`../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md`](../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md); adopted pricing ADRs per §1.2.
- **Related slices**: [`01-foundation.md`](./01-foundation.md) (pipeline, `ScopeKeyAdapter`, snapshot pre-stamp), [`03-metering-models.md`](./03-metering-models.md) (step 3 consumes the outcome), [`04-overlays-precedence.md`](./04-overlays-precedence.md) (overlay `priceOverlay` stacking), [`07-currency-fx.md`](./07-currency-fx.md) (FX-skip flag), [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) (snapshot replay of this evaluator).
