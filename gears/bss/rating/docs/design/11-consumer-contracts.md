Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Consumer & Integration Contracts (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Pricing (Product Catalog), Subscriptions, Finance, Promotions, Billing, Rating | Downstream: Rating | Owners: BSS Rating team -->

# DESIGN — Consumer & Integration Contracts (Slice 11)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-design-consumer-contracts`

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
  - [4.1 Rating Handoff Contract (normative)](#41-rating-handoff-contract-normative)
  - [4.2 Pricing Read-Model Input Contract (normative)](#42-pricing-read-model-input-contract-normative)
  - [4.3 Subscriptions Input Contract (normative)](#43-subscriptions-input-contract-normative)
  - [4.4 Finance FX Input Contract (normative)](#44-finance-fx-input-contract-normative)
  - [4.5 Promotions Coupon Snapshot Contract (normative)](#45-promotions-coupon-snapshot-contract-normative)
  - [4.6 Billing periodState and Obligation Contract (normative)](#46-billing-periodstate-and-obligation-contract-normative)
  - [4.7 pricingSnapshotRef Segment Map (normative)](#47-pricingsnapshotref-segment-map-normative)
  - [4.8 Canonical Naming (normative)](#48-canonical-naming-normative)
  - [4.9 Contracts & Agreements Input Contract (normative)](#49-contracts--agreements-input-contract-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

The **integration surface** of the Rating gear: every fact that crosses the gear boundary, in
either direction, is contracted here and nowhere else — the Foundation
([`01-foundation.md`](./01-foundation.md) §3.3) delegates the external contracts to this slice,
and the step slices (02–07) consume the inputs these contracts freeze without re-negotiating
them. Seven contracts partition the boundary: one **downstream handoff** — Rating, the in-process
consumer of the `evaluate` outcome envelope (§4.1) — and six **upstream/bidirectional inputs** — the pinned
pricing read model (§4.2), Subscriptions context (§4.3), Finance FX (§4.4), Promotions coupon
snapshots (§4.5), Billing `periodState` (§4.6), and Contracts & Agreements commitments (§4.9 —
added by the 2026-07-31 review, #11: a p1 dependency authoring pool balances, draw order,
`overageRate` and negotiated reserved rates, receiving `CommitmentBalanceEffect`, and owing the
T-D-10/T-D-27 re-resolution triggers had no contract row while 05 §3.5 pointed here for one)
([`../PRD.md`](../PRD.md) §9).

Two rules shape the surface. **Adopt verbatim**: whatever the pricing gear owns — the 8-axis key,
the `modelKind` set, `prorationBasis`, `billingAnchorPolicy`, band shapes — arrives as published,
CI-gated against drift; no local re-declaration exists to diverge (SEAMS C1/P1). **One writer per
fact**: every boundary fact has exactly one producing gear, and Rating writes back to none — it
reads frozen inputs, records lineage, and seals exactly its own `pricingSnapshotRef` segments
(SEAMS S1). A missing required input has no default: evaluation fails closed at the boundary
([`../PRD.md`](../PRD.md) §7.1, §9.2).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-interface-tariff-evaluation` | The §9.1 conceptual contract realized as the in-process Rating handoff (§4.1): frozen context in → resolved outcome + sealed `pricingSnapshotRef` + evaluation metadata (applied coupons, `tierAggregationWindow`, `fxTableVersion`, granularity) out; replay-safe per 01 §4.2; incompatible changes take a major version bump — snapshot semantics are part of the contract. |
| `cpt-cf-bss-rating-contract-rating-handoff` | §4.1: the full outcome envelope (rates, model kind, tier thresholds, overlay stack lineage, coupon pre/post amounts, FX record, obligations, ASC 606 refs, rounding-policy id) + the stable `{skuId, planId, priceId}` triple on every emission; Rating maps to RatedCharge/BillableItem and owns persistence, dedup, and the windowed `Q`. |
| `cpt-cf-bss-rating-contract-pricing-readmodel` | §4.2: pin-one-`CatalogVersion` read discipline; all four `PriceWindow*` events + `CatalogVersionPublished` (W1); enums adopted verbatim under the `pricing.contracts.enum_drift` CI gate (P1/P2); the snapshot-replay guarantee (W2); bundle `sum_of_parts` summing with `effective_share_bp` pass-through (B1). |
| `cpt-cf-bss-rating-contract-subscriptions-input` | §4.3: `phase_id` at `t`, eligibility inputs (`activatedAt`, bound cohort via the pinned price id), the seat count behind `per_unit`, `(changeEffectiveAt, changeMode)` consumed — never decided, and the frozen `(currency, region)` binding segment (S1). |
| `cpt-cf-bss-rating-contract-finance-fx-input` | §4.4: FX tables + lock policies versioned by `fxTableVersion` — a member of the determinism tuple; per-window rate-lock and invoice-period modes; a needed conversion without a policy/table record fails closed. |
| `cpt-cf-bss-rating-contract-promotions-coupon` | §4.5: the frozen coupon-snapshot field set; Rating applies and records, never mutates redemption state; missing `applyScope` (or `stackSequence` under `ordered_stack`) fails closed — as does `applyScope = line_total` at launch (T-D-22: Promotions writes an explicit line scope per launch coupon). |
| `cpt-cf-bss-rating-contract-billing-periodstate` | §4.6: `periodState ∈ {open, closed_posted}` as a required input (missing fails closed); full-precision amounts + `PeriodFloorCapObligation` outbound; Billing executes floor/cap and all rounding. |
| `cpt-cf-bss-rating-fr-snapshot-carry` | §4.7: the canonical four-writer segment map reproduced identically from 01 §4.3 / SEAMS S1 — each contract writes only its own segments; Rating (composition SoR) seals the ref at emission. |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-throughput-latency` | §4.2 pin discipline + cache | The read-path cost is the read-model pin: pricing publishes contract fields as columns of existing read-model rows (no extra read-path lookups); events only invalidate the non-authoritative cache | Load test; **targets provisional — NFR workshop** ([`../PRD.md`](../PRD.md) §7.1) |
| `cpt-cf-bss-rating-nfr-resilience` | All six contracts | Fail-closed absence semantics per contract — missing `periodState`, coupon policy, FX record, or a torn snapshot pre-stamp never yields a guessed price; retries replay the 01 §4.2 keys | Chaos/retry test |
| `cpt-cf-bss-rating-nfr-audit-segregation` | §4.2 publish-side duty | Rating's four publish-time checks run as registered fail-closed validators inside the pricing Slice 5 engine — one workflow, one audit trail (validators owned by slice 10) | Joint publish-path test |
| Decimal precision of emitted amounts | §4.6 | Amounts cross the Billing boundary at full intermediate precision; the concrete DECIMAL precision is fixed in this Design before lock ([`../PRD.md`](../PRD.md) §4.1) | **Open — set with Billing** |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-bss-rating-adr-scope-key-adoption` | Adopt the pricing 8-axis canonical scope key verbatim (selection + non-overlap); cohort generation selected by the pinned price id; no Rating-local key — the §4.2 read model is consumed on exactly this key (SEAMS K1–K5). |
| `cpt-cf-bss-pricing-adr-canonical-scope-key` (adopted) | The key definition itself — the manifest key extended additively; the pricing gear is its SoR. |
| `cpt-cf-bss-pricing-adr-grandfathering-cohort-axis` (adopted) | `cohort` = the cutover instant; Rating resolves the generation by the cohort of the subscription's pinned price id (§4.3 eligibility inputs). |
| `cpt-cf-bss-pricing-adr-pricewindow-consolidation` (adopted) | `PriceWindow*` events are produced by the pricing gear; Rating consumes all four (incl. `Cancelled`) as read-only resolution inputs (§4.2). |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-cc`

```text
Upstream SoRs            Pricing (pinned read model · PriceWindow* ×4 · CatalogVersionPublished) ·
                         Subscriptions · Finance (FX) · Promotions (coupons) · Billing (periodState)
        ▼  frozen inputs (§4.2–§4.6)
Boundary (this slice)    contract shapes · read-model pin + input-verification adapters ·
                         enum-conformance CI gate · snapshot segment-writer rules (§4.7)
        ▼
Evaluation pipeline      steps 1–9 over the frozen context (01; step slices 02–07)
        ▼  outcome envelope (§4.1)
Rating (downstream)      RatedCharge/BillableItem mapping · persistence · dedup · windowed Q
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | Boundary adapters: read-model pin, context-input verification, outcome handoff | Rust modules in the `rating` gear (rating-core crate) |
| Domain | The six contract shapes + the `pricingSnapshotRef` segment-writer rules | Rust; GTS + Rust domain structs |
| Infrastructure | Event consumption (`PriceWindow*`, `CatalogVersionPublished`) invalidating the 01 §3.7 cache; build-time enum-conformance fixtures | In-process cache (Foundation-owned); CI gate `pricing.contracts.enum_drift` |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Adopt verbatim, gate the drift

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-adopt-verbatim-cc`

Enums, the scope key, model kinds, and band shapes are the pricing gear's; Rating consumes them
exactly as published, and the `pricing.contracts.enum_drift` CI gate (Critical) blocks any build
whose adopted enums diverge (SEAMS P1; pricing design 06 §7).

#### One writer per boundary fact

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-single-writer-cc`

`Q` is Rating's, redemption state Promotions', FX tables Finance's, `periodState` Billing's, the
`(currency, region)` binding Subscriptions', the catalog pricing's. Rating reads frozen facts,
records lineage, and writes exactly its four eval-time snapshot segments (§4.7) — never a
foreign fact.

#### Frozen in, full precision out

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-frozen-in-precision-out-cc`

Every inbound fact is a frozen snapshot or version; every outbound amount leaves at full
intermediate precision with discount lineage and the rounding-policy id. Execution — rounding,
floor/cap, posting — is downstream's (01 §4.4).

### 2.2 Constraints

#### In-process handoff, event-only asynchrony

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-inprocess-boundary-cc`

The Rating handoff is an in-process call within the one `rating` gear deployable (01 §3.8), not a
service API or queue; the asynchronous surfaces consumed are the
pricing event set (four `PriceWindow*`), the registry's `CatalogVersionPublished`, **and
Billing's `periodState` transitions** (ingested by the slice-16 `PeriodStateRelay` into the
non-authoritative projection — the same inventory omission class as the D-66 producer fix,
2026-07-31 review #27).

#### No local enum or key extension

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-no-extension-cc`

Any extension of an adopted enum or key axis is a **versioned contract change on the pricing
side** (pricing design 06, K1), never a Rating-local addition; unrecognized values fail the
conformance fixtures at build time, not silently at evaluation time.

#### Rating-side schema confirmed intra-gear before Design lock

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-rating-contract-draft-cc`

Since consolidation (ADR-0002 / T-D-16) the §4.1 handoff is **no longer a cross-gear contract** — it
is the internal core↔pipeline crate API, and the ratifying counterpart is pipeline slice
[`15-rated-output-balance-effects.md`](./15-rated-output-balance-effects.md), not an external
draft/empty Rating PRD (ADR-0002 dissolves review-finding C6). The RatedCharge/BillableItem mapping
in §4.1 is the core-side normative proposal that slice 15 ratifies (intra-gear open, tracked in
[`../DECISIONS.md`](../DECISIONS.md)); it stays intent-stable while slice 15 is authored beyond
skeleton, and incompatible changes take a major version bump (PRD §9.1).

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-cc`

The evaluation shapes (`EvaluationContext`, `EvaluationUnit`, `ResolvedPriceOutcome`,
`pricingSnapshotRef`) are owned by 01 §3.1; this slice fixes **which shapes cross the boundary
and who writes them**:

- **Outcome envelope** (→ Rating, §4.1) — `ResolvedPriceOutcome` + obligations (`TrueUpObligation`: slice 05 shape; `PeriodFloorCapObligation`: slice 09 shape) + discount lineage + evaluation metadata + the sealed `pricingSnapshotRef` + `{skuId, planId, priceId}`.
- **Pinned catalog read model** (← pricing, §4.2) — 8-axis key rows, windows, `modelKind` rows + bands, eligibility/cohort, `prorationBasis`/`billingAnchorPolicy`, prepaid-grant set, bundle component sets + `effective_share_bp`.
- **Subscription context** (← Subscriptions, §4.3) — `phase_id`, `activatedAt` + bound cohort, seat count, `(changeEffectiveAt, changeMode)`, the frozen `(currency, region)` binding.
- **FX policy record** (← Finance, §4.4) — rate tables + lock policy + `fxTableVersion`.
- **Coupon snapshot** (← Promotions, §4.5) — the frozen field set of §4.5.
- **Period context** (← Billing, §4.6) — `periodState` for the period covering `t`.

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-boundary-surface-cc`

- **`ReadModelPinAdapter`** — pins exactly one committed **pin-eligible** `CatalogVersion` per resolution run (monotonic read; `CatalogVersionPublished` + warm-completion marker + **every earlier version itself pin-eligible** — the prefix-closed frontier, pricing D-114, adopted as T-D-31; pin lag ≤ 5s; no draft read — pricing design 01 §4.4); consumes the five **catalog** events (the four pricing `PriceWindow*` + the registry's `CatalogVersionPublished` — the producer split per §3.6; "five pricing events" misattributed one word, review #23) to invalidate the Foundation's resolved-window cache.
- **`ContextInputVerifier`** — per-contract presence/shape verification of the Subscriptions/Finance/Promotions/Billing inputs before pipeline entry; absence surfaces as the fail-closed problem values of 01 §3.3, never a default.
- **`OutcomeHandoff`** — the in-process emission surface to Rating: asserts the rating-compat triple and the sealed ref on every emission (sealing itself is 01's `SnapshotComposer`/`EmissionGuard`; this is where the envelope leaves the gear).
- **Enum-conformance fixtures** (build-time) — the CI-gate fixture set (`pricing.contracts.enum_drift`, Critical) pinning `modelKind`, `prorationBasis`, and `billingAnchorPolicy` byte-identical to the pricing canonical set (SEAMS P1; T-D-07).

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-rating-handoff-cc`

**Rating handoff** (provided; in-process): `evaluate(EvaluationContext) → ResolvedPriceOutcome`
and `reresolve(window, pinnedSnapshotRef, priorRatedVersion) → deltas` (01 §3.3) — envelope,
duties, and idempotency per §4.1.

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-pricing-readmodel-cc`

**Pricing read-model input** (required): the pricing catalog read-model interface
(`cpt-cf-bss-pricing-interface-catalog-read-model`) plus the event set — pin discipline, payload,
and guarantees per §4.2.

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-context-inputs-cc`

**Context inputs** (required): Subscriptions (§4.3), Finance FX (§4.4), Promotions coupons
(§4.5), Billing `periodState` (§4.6) — all frozen at entry, all fail-closed on absence; the PRD
§9.2 contract ids map one-to-one onto the §4 subsections (§1.2 Functional Drivers).

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-contracts-input-cc`

**Contracts & Agreements input** (required where a line carries pools or a negotiated
reservation): the commitment surface — pool set + balances @ `balanceVersion` in, negotiated
reserved rates via the step-5 overlay, `CommitmentBalanceEffect` out, T-D-10/T-D-27
re-resolution triggers in — per §4.9.

### 3.4 Internal Dependencies

Everything — this slice formalizes the boundary the whole set evaluates behind:
[`01-foundation.md`](./01-foundation.md) owns `evaluate`/`reresolve`, composition, and the
emission guards; [`05`](./05-commitments-reservations.md)/[`09`](./09-period-plan-change.md) own
the obligation shapes the §4.1 envelope carries; [`06`](./06-coupons.md)/[`07`](./07-currency-fx.md)
evaluate the §4.5/§4.4 policy semantics; [`08`](./08-retroactivity-corrections.md) exercises the
§4.2 snapshot-replay guarantee; [`10`](./10-governance-asc606.md) owns the registered validators
and the rev-share pass-through recorded in §4.2.

### 3.5 External Dependencies

| Dependency | What crosses the boundary | Contract |
|------------|--------------------------|----------|
| Pricing (Product Catalog) | pinned read model; `PriceWindow*` ×4 + `CatalogVersionPublished`; bundle component sets + `effective_share_bp` | §4.2; SEAMS C1, W1, W2, P1/P2, B1, M11 |
| Rating pipeline (intra-gear — slices 12–16) | outcome envelope out; windowed `Q`, usage dedup, rated-output persistence stay pipeline-side | §4.1; SEAMS M7; slices 12–16 |
| Subscriptions | `phase_id`, eligibility inputs, seat count, `(changeEffectiveAt, changeMode)`, `(currency, region)` binding | §4.3; SEAMS S1 |
| Finance | FX tables + lock policies, `fxTableVersion` | §4.4 |
| Promotions | frozen coupon snapshots | §4.5 |
| Billing | `periodState` in; obligations + full-precision amounts out | §4.6 |
| Contracts & Agreements | pool set (`poolType`, balances @ `balanceVersion`, draw order, rollover, `overageRate`) in + negotiated reserved rates (step-5 overlay) in; `CommitmentBalanceEffect` out; T-D-10/T-D-27 re-resolution triggers in | §4.9; SEAMS M8/M9; T-D-10/14/19/27 |

### 3.6 Interactions and Sequences

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-flow-evaluate-handoff-cc`

**Evaluate and hand off one line** (boundary view of `cpt-cf-bss-rating-seq-evaluate-tariff`):

1. Rating assembles the `EvaluationContext` from the five upstream inputs (§4.2–§4.6) plus the evaluation unit it owns (normalized `UsageRecord` or windowed `Q`).
2. `ReadModelPinAdapter` pins the committed `CatalogVersion`; `ContextInputVerifier` fails closed on any absent required input.
3. The Foundation pipeline evaluates steps 1–9 (01 §3.6).
4. `OutcomeHandoff` emits the §4.1 envelope in-process; Rating maps it to RatedCharge/BillableItem, persists, and dedups on the usage idempotency key (01 §4.2).

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-pricewindow-events-cc`

**Consume the pricing event set** (SEAMS W1):

1. **Two producers, two streams** (corrected 2026-07-29 cross-gear review, pricing D-66 — the earlier text attributed both to pricing's outbox): (a) the **pricing** gear emits `PriceWindowScheduled/Activated/Expired/Cancelled` from its outbox — at-least-once, idempotency-keyed, ordered per `(tenant, plan)` (pricing design 07 §7); `CatalogVersionPublished` is **not** in pricing's frozen event-name set (pricing `fr-event-contract`). (b) `CatalogVersionPublished` is emitted by the **registry (products gear)**, which is the **sole** `CatalogVersion` incrementer (products `fr-sku-catalog-version`; pricing design 01 §3 step 5 — the Foundation only *requests* addressability on publish). It is a tenant-wide catalog-publish event, **not** per-plan: rating MUST subscribe to it on the registry's stream and MUST NOT assume `(tenant, plan)` ordering for it. Wiring the pin adapter to pricing's topic alone would mean no `CatalogVersion` ever becomes pin-eligible (step 3) and every resolution run failing closed.
2. Each event invalidates the matching pages of the non-authoritative resolved-window cache (01 §3.7) — **including `Cancelled`**, so a pre-cached scheduled window that pricing later voids is retracted (the W1 failure this contract closes).
3. A `CatalogVersion` becomes pin-eligible only after `CatalogVersionPublished`, the warm-completion marker, **and every earlier version being pin-eligible** (the prefix-closed frontier — pricing D-114 / T-D-31; §4.2); events carry no rate authority — resolution always reads the pinned model.

### 3.7 Database Schemas and Tables

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-storage-none-cc`

**None owned.** This slice adds no store to the Foundation doctrine (01 §3.7); it contributes
only the invalidation triggers for the non-authoritative cache (`PriceWindow*` ×4,
`CatalogVersionPublished`). Authoritative state per boundary fact: catalog → pricing gear;
`Q`/dedup/rated output → Rating; phase structure and change policy → Subscriptions; FX →
Finance; coupon entities and redemption → Promotions; period and rounding → Billing.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-cc`

**None beyond the Foundation's.** rating-core rides the one `rating` gear deployable (01 §3.8), so the §4.1
handoff crosses no process boundary; the §4.2 events are the only cross-deployable transport this
slice consumes; the enum-conformance fixtures run as a build-time block (pricing design 06 §7).

## 4. Additional Context

### 4.1 Rating Handoff Contract (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-rating-handoff-cc`

**Direction**: provided to Rating, in-process (01 §3.8). **Contract**:
`evaluate(EvaluationContext) → ResolvedPriceOutcome` + `reresolve → deltas` (01 §3.3) —
replay-safe: same context + same frozen inputs ⇒ byte-identical outcome. Every emission carries
([`../PRD.md`](../PRD.md) §9.1–§9.2; 01 §3.1, §4.4):

- resolved effective rates, model kind, tier thresholds, overlay winners + stack lineage;
- applied coupon id(s) + pre-/post-discount amounts, and the FX policy record (`fxTableVersion` / locked-rate id);
- the obligations envelope — `TrueUpObligation` (shape: slice 05) and `PeriodFloorCapObligation` (shape: slice 09) — surfaced for Billing execution, never posted by Rating;
- discount lineage (pre/post-overlay, pre/post-coupon + applied ids) for Billing/Tax gross-vs-net and audit; ASC 606 refs `performanceObligationRef`/`sspSnapshotPointer` — **null at MVP** (SEAMS ASC), with `glCode` passing through as supplied frozen evidence (PRD §14). **Billing-side continuation (mirror of pricing D-48, 2026-07-28):** prepaid-grant drawdown applies to the **post-discount, pre-tax** amount — a credit reduces the charge, never pays the invoice; dormant at MVP (tax-exclusive), revisit checkpoint at Tax Engine GA; the lineage above is what makes either audit reading computable, the D-48 line is what selects the normative one;
- evaluation metadata (applied coupons, `tierAggregationWindow`, `fxTableVersion`, granularity — PRD §9.1), the **sealed** composed `pricingSnapshotRef` (§4.7), the rounding-policy id (01 §4.4), and the stable rating-compat triple `{skuId, planId, priceId}` — ids never re-used across revisions (pricing design 06, rating-compatibility bundle).

**Outcome → Rating mapping** (normative Rating-side proposal — ratified by the Rating design
before Design lock, §2.2):

| Outcome element | Rating-side mapping |
|---|---|
| resolved line (usage / recurring / `capacityCharge`) | one `RatedCharge` per charge line and unit, carrying the full-precision amount + rounding-policy id; `BillableItem.pricingSnapshotRef` = the sealed ref; `{skuId, planId, priceId}` verbatim |
| zero-due `prepaid_drawdown` in-commit line (slice 05 §4.1, T-D-14) | a `RatedCharge` with amount-due 0 + notional value in lineage — itemized, never dropped |
| obligations (`TrueUpObligation`, `PeriodFloorCapObligation`) | envelope ride-along to Billing execution — never a `RatedCharge` |
| provisional invoice-period-FX amount (slice 07) | `RatedCharge` flagged provisional; the close-time delta supersedes by correction key, never mutates |
| correction deltas (slice 08) | `Adjustment` entities keyed by the correction key `(unitKey[, slice], prior-rated-version, snapshot)` (01 §4.2 — both unit families); the original `RatedCharge` is immutable |
| idempotency | usage/period key ⇒ `RatedCharge` dedup (both unit families — the ratifying slice 15 wording; review #29); correction key ⇒ `Adjustment` dedup — both enforced by Rating (T-D-11) |
| `CommitmentBalanceEffect` (slice 05 §4.1, T-D-10) | published by Rating to Contracts per rated outcome, idempotent on the outcome's key |

**Duties** (PRD §9.2; SEAMS M7): Rating owns the Usage → RatedCharge pipeline, **rated-output
persistence**, **usage dedup** (authoritative), and the windowed `Q` per
`(subscription, meter, dimensionKey, window)` — single writer per key, including the per-slice
attribution and `bandOffsetQ` of a split window (slice 03 §4.3, T-D-12); Rating never
aggregates. Rating additionally owns the **period tick** that synthesizes the period-driven
evaluation units — recurring lines, capacity-flavor charges, true-up surfacing — at
`AnchorPeriod` boundaries (01 §4.2, T-D-15), and **publishes `CommitmentBalanceEffect`s to
Contracts** (T-D-10). `reresolve` deltas carry the correction key
`(unitKey[, slice], prior-rated-version, snapshot)`; **delta dedup is Rating's (T-D-11)** —
01 §2.2. The mapping table above is the core-side proposal ratified intra-gear by pipeline slice 15 (§2.2
constraint — confirmation open, no longer a cross-gear contract since ADR-0002); incompatible
changes take a major version bump (PRD §9.1).

### 4.2 Pricing Read-Model Input Contract (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-pricing-readmodel-cc`

**Direction**: required from Pricing (Product Catalog) — the Rating side of the frozen pricing
consumer contract
([`06-consumer-contracts.md`](../../../pricing/docs/design/06-consumer-contracts.md); SEAMS C1).

- **Read model (pinned)**: rows on the 8-axis canonical scope key with UTC half-open windows (non-overlap on the full key); `modelKind ∈ {flat, per_unit, graduated, volume, package}` with the pricing §17.2 kind → formula mapping as shared SoR (T-D-05); tier bands; `priceEligibility` + `cohort` generation; `prorationBasis ∈ {calendar_days_actual, calendar_days_30, by_second, whole_unit, none}`; `billingAnchorPolicy ∈ {calendar_month, subscription_start, fixed_day(d)}` with the D-20 last-of-month clamp (anchor day preserved, no drift); the prepaid-grant set (plan-attached credit grants whose balance/drawdown executor is Billing/Rating — distinct from `commitmentPools[]`, SEAMS M8); the rating-compat triple.
- **Pin discipline** (pricing design 01 §4.4): one committed `CatalogVersion` per resolution run; a version is pin-eligible only after `CatalogVersionPublished`, its warm-completion marker, **and every earlier version being itself pin-eligible — pin-eligibility is version-level and prefix-closed, a monotonic frontier (pricing D-114, adopted as T-D-31: without the prefix a stuck older version's late warm made one pin resolve two contents over time — the replay divergence one version out)**; pin lag ≤ 5s; no draft read; no default substitution.
- **customerGroup resolution (review #40)**: the payer's customer group at `t` — the step-4 `customerGroup` scope input (slice 04 §4.1) — resolves from the pinned read model's **membership subject** (pricing D-91/D-06 units) at context assembly and freezes into the evaluation context; no caller claims are read at evaluation, and a period-tick-synthesized unit resolves it identically (assembly rule: slice 14 §4.3).
- **Enums verbatim (P1/P2, T-D-07)**: adopted byte-identical under CI gate `pricing.contracts.enum_drift` (**Critical**) — drift is a build-time block; the runtime alarm covers registry divergence (pricing design 06 §7). Rating never prorates a `none` row (pricing rejects `creditOnDowngrade = true` + `none` at publish) but MUST recognize the value for the conformance fixture (SEAMS P1). The plan-change fields (`allowedChangeTargets`, `comparabilityRank`) are published for Subscriptions; Rating's plan-change inputs are §4.3's `(changeEffectiveAt, changeMode)` plus these frozen enums.
- **Events (W1)**: **all four** `PriceWindowScheduled/Activated/Expired/Cancelled` from the **pricing** outbox, plus `CatalogVersionPublished` from the **registry (products gear)** — the sole `CatalogVersion` incrementer, tenant-wide and not per-plan (corrected 2026-07-29, pricing D-66); activation-job events at-least-once, idempotency-keyed, ordered per `(tenant, plan)` (pricing design 07 §7). Consuming `Cancelled` is load-bearing: it retracts pre-cached scheduled windows that pricing voids.
- **Snapshot-replay guarantee (W2, T-D-04)**: pricing retains snapshots for open windows; every correction — open-period late arrival and posted-period alike — replays the pinned `pricingSnapshotRef`, never a live catalog read.
- **Bundles (B1, T-D-08)**: for `sum_of_parts` the catalog persists the component `planId` reference set only — **the summing is Rating's at eval**; rev-share arrives publish-normalized (`SUM(effective_share_bp) + platform_cut_bp = 10000` exactly, pricing D-07) and Rating reads **only** `effective_share_bp`, passing it through untouched (evaluated in slice 10).
- **Catalog guarantees relied on (M11)**: open-top tier bands (D-17 — no fail-closed-above-max branch exists); graduated/volume/package restricted to `chargeKind = usage` (D-18); grandfathered rows immutable in price.
- **Publish-side duty (G1, T-D-06)**: Rating registers its four publish-time checks as fail-closed validators inside the pricing Slice 5 approval engine — a single workflow, never a second one (validators owned by slice 10; the registration is part of this contract, PRD §9.2).

### 4.3 Subscriptions Input Contract (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-subscriptions-input-cc`

**Direction**: required from Subscriptions ([`../PRD.md`](../PRD.md) §9.2).

- **`phase_id`** — the active plan phase at `t` (uuid; kind names trial/intro/evergreen are display-only — pricing D-19 via T-D-01); the step-1 input.
- **Eligibility inputs** — `activatedAt` plus the bound cohort **via the pinned price id** in `pricingSnapshotRef` (never `activatedAt` alone — 01 §4.1); step-2 inputs.
- **Seat count / manual quantity** — the quantity behind `per_unit` rows: `quantitySource = subscription_seat_count` supplies the seat count; `quantitySource = manual` supplies an **authored subscription attribute** — both arrive in the same frozen context field from Subscriptions (SEAMS M2).
- **Plan change** — `(changeEffectiveAt, changeMode)`: Rating consumes the pair to pick the split point and rates each side against its own revision and snapshot (PRD §17.2); **the mode-setting policy is Subscriptions' — Rating never decides the mode**.
- **`(currency, region)` binding** — written by Subscriptions into `pricingSnapshotRef` **at activation**, frozen thereafter: the one snapshot segment a third gear writes (§4.7; SEAMS S1).
- **The period-fact stream (T-D-33, 2026-08-01 — the recurring WHEN)** — `BillableItemCreated(kind=recurring)`: money-free facts per `(subscriptionId, billing period, lineKey)`, one per component interval, carrying the per-component traceability tuple, `pricingSnapshotRef`, the suspended interval(s) + `pause_recurring | continue` posture, the period-start `payerTenantId`, and the `collectionPaused` marker; consumed on the shared ordering key as the **synthesis trigger** for every period-driven unit (slice [`14`](./14-unit-synthesis-period-tick.md) §4.2). This gear never derives a recurring period itself; absence past the calendar watchdog is a §17.1 alarm, never self-synthesis.
- **Quote-time (subscription-less) contexts** — e.g. bundle `sum_of_parts` summing at quote — carry none of the above inputs; evaluation defaults to the plan's terminal `phase_id`, `priceEligibility = all_subscriptions`, `cohort = none`, no proration (slice 02 §4.3); the `(currency, region)` pair comes from the quote request rather than an activation-frozen binding, and the quote outcome is an **estimate surface**, not a rateable emission.

### 4.4 Finance FX Input Contract (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-finance-fx-cc`

**Direction**: required from Finance ([`../PRD.md`](../PRD.md) §9.2). FX rate tables + lock
policies, versioned by `fxTableVersion` — a member of the determinism tuple
`(window-aggregated inputs, pricingSnapshotRef, fxTableVersion)` (01 §4.2). Two policy modes
(step 8, PRD §17.1): **per-window rate-lock** (final at event time) and **invoice-period FX**
(provisional on the hot path; delta re-rate at period close — close-time `fxTableVersion`
authoritative). Inputs are immutable frozen records; rating-core records `fxTableVersion` / locked-rate
id, and the FX-lock id is a Rating-written snapshot segment (§4.7; slice 07). **No implicit or
provider-default FX exists** — a needed conversion with no policy/table record fails closed (PRD
§7.1, §9.2).

### 4.5 Promotions Coupon Snapshot Contract (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-promotions-coupons-cc`

**Direction**: required from Promotions ([`../PRD.md`](../PRD.md) §9.2, §17.2).

- **Frozen snapshot fields (at minimum)**: `couponId`, `adjustmentType` (`percent | fixed_amount`), `value`, `valueCurrency` (required for `fixed_amount` — slice 06 §4.4, review #31), `settlementCurrency` (`price | billing`), `applyPerTierBand`, `applyScope` (`usage | recurring | line_total`, default `line_total`), `stackSequence` (required under `ordered_stack`, unique within a linked set), validity, applicability filters, redemption eligibility.
- **Launch rule (T-D-22 — review #12)**: **Promotions MUST write an explicit line scope (`usage` | `recurring`) for every launch coupon.** The documented authoring default `line_total` **fails closed at evaluation** for want of a plan-scoped base (slice 06 §4.2/§4.3) — never applied to one line, never downgraded — so a launch coupon shipped on the default is a coupon that never discounts. This is the obligation the slice-06 counterpart states on the Rating side; it belongs in this contract because this section is what a Promotions engineer builds against, and the §4.5 "Open" below is the named alignment venue.
- **Stacking**: `exclusive_best` (default) or `ordered_stack` (campaign-linked only); incompatible pairs fail closed at redemption bind (Promotions-side). `settlementCurrency` fixes the slot: price-currency coupons at step 7 before FX; billing-currency coupons after step 8 under the same `fxTableVersion` (PRD §17.1).
- **Duties**: Rating **applies and records** — applied ids + pre/post amounts into the lineage and its snapshot segment — and **never mutates redemption state**; coupon lifecycle, campaigns, and redemption counters are Promotions' (PRD §5.2). Missing `applyScope`, or missing `stackSequence` under `ordered_stack`, fails closed.
- **Open**: the Promotions PRD does not exist yet — field names and the snapshot event contract must align with PRD §17.2 before production coupon rating (PRD §13, §15); the semantics above are the normative Rating-side stub meanwhile.

### 4.6 Billing periodState and Obligation Contract (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-billing-periodstate-cc`

**Direction**: bidirectional with Billing ([`../PRD.md`](../PRD.md) §9.2).

- **Inbound**: `periodState ∈ {open, closed_posted}` for the period covering `t` — a required context field; **missing fails closed**. `open` routes late/corrected usage through pinned-snapshot re-resolution; `closed_posted` engages posted-period, delta-only protection (slice 08).
- **Outbound**: full-precision sub-window amounts plus `PeriodFloorCapObligation` (amount, currency, scope — slice 09 shape; floor/cap set in price currency and converted under the step-8 FX policy, PRD §17.2).
- **Duties**: Billing aggregates the period, executes `max(total, floor)` / `min(total, cap)`, and applies **all** rounding; Rating never rounds and never applies period-level min/max — Billing owns the rounding policy, and the emission records its id (01 §4.4). The non-negative guard runs before floor/cap; a floor never masks a negative line.
- **Opens recorded inline**: the concrete DECIMAL precision of emitted amounts is fixed with Billing in this Design before lock (§1.2); whether a contractual floor claws back coupon discount is a PRD §15 open (default proposal: the floor compares the post-coupon total).

### 4.7 pricingSnapshotRef Segment Map (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-snapshot-segments-cc`

At the boundary, the composition contract of 01 §4.3 is a **cross-gear write protocol**: one ref,
**four writers** (registry, pricing, Subscriptions, Rating — "three writers" predated D-66's
producer split, under which `catalogVersion` is the registry's; the table below always had four,
review #10), non-overlapping segments; Rating is the composition SoR (SEAMS S1, T-D-03) and
seals the ref at emission — immutable thereafter. The canonical segment table — recorded
identically in 01 §4.3, [`../SEAMS.md`](../SEAMS.md) S1, and here:

| Segment | Writer | When |
|---------|--------|------|
| `catalogVersion` (pending → committed) | **registry (products gear)** — sole incrementer; pricing only requests addressability | pricing publish → registry `CatalogVersionPublished` |
| resolved price ids (**incl. `cohort`**) | pricing gear | publish |
| evaluation-policy version (incl. the T-D-17 level-aggregation triple — `aggregationFunction`, `aggregationGranularity`, `maxHold` — per 01 §4.3; review #10: this row had omitted it) | pricing gear | publish |
| `(currency, region)` binding | Subscriptions | activation |
| resolved overlay / `priceOverlay` ids | Rating | evaluation |
| applied coupon id(s) + stacking policy | Rating | evaluation |
| FX-lock id (if any) | Rating | evaluation |
| `commitmentReservation` — reservation match (id, flavor, `reservedQuantity`, resolved rate source); pool set (per-pool id, unit, `poolType`, balance-as-of + `balanceVersion`, draw order, rollover, optional **`overageRate`** — the T-D-19 residual-pricing selector; review #5); reserved-vs-pool split | Rating | evaluation |

Per-writer obligations: pricing pre-stamps its **two** segments at publish (the `catalogVersion`
row is the registry's since D-66 — "its three segments" was the pre-split count, review #10) and
its catalog-side
view **MUST NOT diverge** from the Rating-composed ref (pricing design 01 §4.4); Subscriptions
writes the binding exactly once, at activation (§4.3); Rating appends its four eval-time
segments and seals. A context whose pre-stamp or binding is missing or torn is rejected
fail-closed — no segment is ever fabricated (01 §4.3).

**Resolved (2026-07-11, T-D-09)**: the step-6 frozen identifiers form the **eighth named
segment** `commitmentReservation` (row above) — writer Rating @ evaluation (no new writer, no
pricing-side change), recorded identically in [`01-foundation.md`](./01-foundation.md) §4.3,
[`05-commitments-reservations.md`](./05-commitments-reservations.md) §4.1,
[`../SEAMS.md`](../SEAMS.md) S1, and here. The segment's `balanceVersion` is the frozen
balance-sequencing point of T-D-10.

### 4.8 Canonical Naming (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-canonical-naming-cc`

SEAMS N1: the canonical name of the upstream catalog gear is **"Pricing (Product Catalog)"**;
"Catalog / Price Book" and bare "Product Catalog" are superseded aliases in this design set.
The pricing gear names this consumer `cpt-cf-bss-pricing-actor-rating` (pricing design 06 §1.3;
renamed to the rating actor with the consolidation — ADR-0002 commit C). On the rating side
([`../PRD.md`](../PRD.md) §2.1): **Rating** names the gear and domain, **rating-core** the pure
evaluation crate, and **Tariffs / PLAL / tariff-core** are deprecated (ADR-0002); "tariff" is
reserved for Pricing-owned rate definitions. A reference through a superseded alias is a
documentation defect, not a second system.

### 4.9 Contracts & Agreements Input Contract (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-contracts-input-cc`

**Direction**: bidirectional with Contracts & Agreements ([`../PRD.md`](../PRD.md) §9.2; a `p1`
dependency). Added by the 2026-07-31 review (#11): the semantics below were designed across
slices 05/14/15 while §1.1 claimed six contracts partitioned the boundary and
[`05-commitments-reservations.md`](./05-commitments-reservations.md) §3.5 pointed here for a
contract row that did not exist — a completeness and routing gap, not missing design.

- **Inbound (frozen at context assembly into the `commitmentReservation` segment, §4.7)**: the
  ordered pool set — per-pool id, unit, `poolType ∈ {prepaid_drawdown, committed_rate}`
  (T-D-14), balance-as-of + `balanceVersion` (T-D-10 serialization), draw order, rollover
  policy, and the optional `overageRate` (T-D-19) — plus the contract's period true-up clause
  (`commitmentBasis`, committed quantity/spend — slice 05 §4.5).
- **Inbound (via the step-5 contract overlay, not this segment)**: negotiated RI-style reserved
  rates — the M9 two-source rule (slice 05 §4.3); contract-level overrides stay governed by
  Contracts (SEAMS G1).
- **Outbound**: one `CommitmentBalanceEffect` per **pool-observing** outcome — draw, refill, or
  zero-draw + overage marker (T-D-10 coverage completed by T-D-27) — idempotent on the
  outcome's evaluation/correction key; `TrueUpObligation` rides the Billing handoff (§4.6),
  never this contract.
- **Inbound triggers**: Contracts serializes per-pool `balanceVersion`, and emits re-resolution
  triggers for later-`balanceVersion` units after a balance-affecting correction (T-D-10) and
  for over-drawn units detected at write-back (T-D-27 — aggregate draws at one frozen
  `balanceVersion` exceeding the balance); Rating routes each through `reresolve`, delta-only,
  each unit under its own pin (slice 08 §4.4).
- **Fail-closed**: a spend pool whose denomination differs from the line's price currency
  (slice 05 §4.1); an unknown `poolType`; a line whose snapshot claims pools with no pool set in
  context. Denomination coverage and pool authoring are Contracts-side publish obligations.

## 5. Traceability

**Traces to**: `cpt-cf-bss-rating-fr-snapshot-carry`

- **PRD**: §9.1 (evaluation contract), §9.2 (all six external contracts), §6.1 (`fr-snapshot-carry`, `fr-idempotency`, `fr-separation` — the boundary-visible determinism duties), §17.1 steps 8–9 (FX/emission boundaries), §17.2 (coupon / floor-cap / plan-change boundary contracts), §13 (dependencies), §2.1 (naming), §4.1 (environment constraints).
- **Seams**: C1 (this contract), S1 (segment map), B1 (bundle summing + rev-share pass-through) — owned here; W1 (four events), W2 (snapshot replay), M7 (windowed `Q` key), P1/P2 (enum adoption + CI gate), N1 (naming) surface in these contracts — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-01 (adopted key in the read model), T-D-03 (composition SoR), T-D-04 (replay + counter key), T-D-05 (modelKind set), T-D-06 (validator registration), T-D-07 (enum CI gate), T-D-08 (rev-share pass-through) — [`../DECISIONS.md`](../DECISIONS.md).
- **ADR**: [`../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md`](../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md).
- **Pricing design (adopted SoR)**: [`06-consumer-contracts.md`](../../../pricing/docs/design/06-consumer-contracts.md), [`07-pricewindow-linkage.md`](../../../pricing/docs/design/07-pricewindow-linkage.md), [`01-foundation.md`](../../../pricing/docs/design/01-foundation.md), [`08-bundles.md`](../../../pricing/docs/design/08-bundles.md).
- **Related slices**: [`01-foundation.md`](./01-foundation.md) (pipeline, composition, guards), [`05`](./05-commitments-reservations.md)/[`09`](./09-period-plan-change.md) (obligation shapes), [`07`](./07-currency-fx.md) (FX policy), [`08`](./08-retroactivity-corrections.md) (replay), [`10`](./10-governance-asc606.md) (validators + rev-share).
