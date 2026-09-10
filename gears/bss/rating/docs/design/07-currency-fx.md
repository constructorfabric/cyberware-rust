Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Multi-Currency & FX (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Finance (FX), Pricing (Product Catalog), Subscriptions, Promotions | Downstream: Rating, Billing | Owners: BSS Rating team -->

# DESIGN — Multi-Currency & FX (Slice 7)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-design-currency-fx`

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
  - [4.1 Currency Role Separation (normative)](#41-currency-role-separation-normative)
  - [4.2 FX Policy Semantics (normative)](#42-fx-policy-semantics-normative)
  - [4.3 FX-Lock Snapshot Segment (normative)](#43-fx-lock-snapshot-segment-normative)
  - [4.4 Ordering and Precision at the FX Boundary (normative)](#44-ordering-and-precision-at-the-fx-boundary-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice is the **step-8 evaluator** of the §17.1 order ([`../PRD.md`](../PRD.md) §6.9, §17.1):
the only point in the platform where an authoritative currency conversion happens. It keeps three
currency roles strictly apart — **price currency** (the selected row's currency; per-market rows
are first-class catalog rows, never FX-derived), **billing currency** (the payer's invoice
currency, frozen by Subscriptions at activation), and **presentment currency** (portal display FX,
non-authoritative and outside rating-core) — and converts exactly once, only when billing ≠ price.

Conversion runs as a pure function over **frozen Finance inputs**: the FX table and lock policy
arrive with a `fxTableVersion` that is part of the determinism tuple
([`./01-foundation.md`](./01-foundation.md) §4.2). Two deterministic policies exist and no third:
**per-window rate-lock** (final at event time) and **invoice-period FX** (provisional on the hot
path, authoritative re-rate **by delta at period close** under the slice-08 correction keys —
never by mutating the provisional line). The FX-lock id is a Rating-written segment of
`pricingSnapshotRef` (SEAMS S1); conversion never rounds — Billing rounds in billing currency
after conversion; a missing FX record where one is required fails closed, never a provider default.

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-fr-multi-currency` | Three declared roles (§4.1): price (selected row; distinct per-market rows, never FX-derived), billing (payer invoice currency via the frozen Subscriptions binding), presentment (display-only, outside rating-core, labelled estimates). The `CurrencyRoleResolver` converts only when billing ≠ price; equal currencies skip step 8 (§17.1 step 2, native multi-currency). |
| `cpt-cf-bss-rating-fr-fx-policy` | The `FxPolicyApplier` executes exactly two policies over the frozen Finance table (§4.2): per-window rate-lock (final at event time) and invoice-period FX (provisional, flagged; close-time `fxTableVersion` authoritative; delta at close via slice 08). Every conversion records `fxTableVersion` / locked-rate id; no implicit or provider-default rate can be emitted. |
| `cpt-cf-bss-rating-fr-evaluation-order` | The evaluator registers into the fixed step-8 slot: after price-currency coupons (step 7), before emission (step 9); there is no configuration surface to move it. |
| `cpt-cf-bss-rating-fr-coupon-application-order` | The `settlementCurrency` split (§4.4): `price` coupons complete in step 7 before conversion; `billing` coupons apply after conversion on the billing-currency amount under the **same** `fxTableVersion`. The placement is this slice's; the coupon semantics (stacking, applyScope) stay slice 06's. |
| `cpt-cf-bss-rating-fr-snapshot-carry` | The FX-lock id is the Rating-written FX segment of `pricingSnapshotRef` (§4.3, SEAMS S1); the recorded `fxTableVersion` rides the outcome metadata; both immutable once the ref is sealed. |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-throughput-latency` | Invoice-period policy split | The hot path never waits for close-time rates: provisional amount at the locked/spot rate from the frozen table, no I/O inside the step; the authoritative re-rate is an off-hot-path delta | Load test; **targets provisional — NFR workshop** ([`../PRD.md`](../PRD.md) §7.1) |
| `cpt-cf-bss-rating-nfr-resilience` | Fail-closed FX guard | Missing FX table / policy / lock record with billing ≠ price ⇒ fail closed, never a provider-default rate; retries replay the same `fxTableVersion` to the same amount | Chaos/retry test |
| `cpt-cf-bss-rating-nfr-horizontal-scale` | Frozen-table pinning | The table version is pinned per evaluation unit — no shared mutable rate state, no cross-partition coordination | Design + load test |
| Decimal precision of converted amounts | Full-precision conversion | Conversion never rounds (§4.4); Billing rounds in billing currency; the concrete DECIMAL precision is the Foundation open ([`./01-foundation.md`](./01-foundation.md) §4.4) | **Open — set with Billing** |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-bss-rating-adr-scope-key-adoption` | `currency` is an axis of the adopted 8-axis key: each market is its own catalog row selected at step 2 — step 8 never fabricates a missing market row via FX derivation. |
| `cpt-cf-bss-pricing-adr-canonical-scope-key` (adopted) | The key definition carrying the `currency`/`region` axes; per-`(currency, region)` rows are authored independently in the pricing gear ([`../../../pricing/docs/design/04-currency-tax.md`](../../../pricing/docs/design/04-currency-tax.md)). |

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-fx`

```text
Step-8 evaluator (this slice)     CurrencyRoleResolver · FxPolicyApplier · FxLockRecorder ·
        │  (registers into the fixed §17.1     BillingCurrencyCouponPass · FxCloseDeltaCalculator
        ▼   step-8 slot)
Evaluation pipeline (Foundation)  EvaluationPipeline · SnapshotComposer · DeterminismGuard ·
                                  EmissionGuard · MetadataRecorder
        │
        ▼
Frozen inputs (external SoRs)     FX tables + lock policy, fxTableVersion (Finance) ·
                                  per-(currency, region) price rows (pricing) · (currency, region)
                                  binding (Subscriptions) · coupon snapshots (Promotions) ·
                                  periodState at close (Billing)
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | The step-8 evaluator and the close-time re-rate math used by slice 08 | Rust module in the `rating` gear (rating-core crate) |
| Domain | Currency roles, FX policy semantics, `FxApplication` / lock-segment shapes | Rust; GTS + Rust domain structs |
| Infrastructure | **None authoritative** — a non-authoritative cache of frozen FX table pages keyed by `fxTableVersion`; loss degrades latency, never correctness | In-process cache; Rating persistence |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Three roles, one conversion point

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-currency-roles-fx`

Price, billing, and presentment currency are distinct declared roles, never inferred from one
another. The only authoritative conversion in the platform is step 8, and it runs only when
billing ≠ price; presentment FX is never computed here ([`../PRD.md`](../PRD.md) §6.9).

#### No unrecorded FX

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-no-implicit-fx`

Every converted amount carries its policy identity: policy kind + `fxTableVersion` / locked-rate
id. An implicit or provider-default rate is a defect; absence of the required FX record fails
closed ([`./01-foundation.md`](./01-foundation.md) §3.3).

#### Convert at full precision, never round

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-principle-full-precision-conversion-fx`

Conversion output keeps full intermediate precision. Rounding is Billing's, in billing currency,
after conversion (§17.1 rating-core/Billing boundary; [`./01-foundation.md`](./01-foundation.md) §4.4).

### 2.2 Constraints

#### FX tables and policy are Finance's

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-finance-sor-fx`

FX rate tables and lock policies arrive **frozen** with a `fxTableVersion`
([`../PRD.md`](../PRD.md) §9.2 Finance FX input contract). Rating never sources, derives, or
interpolates a rate; the boundary contract is owned by
[`11-consumer-contracts.md`](./11-consumer-contracts.md).

#### The (currency, region) binding is consumed, never re-derived

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-binding-consumed-fx`

Subscriptions freezes the `(currency, region)` binding into `pricingSnapshotRef` at activation
(SEAMS S1). Step 8 reads the bound billing currency from the frozen context; re-deriving it at
evaluation time is a defect.

#### Presentment is outside rating-core

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-constraint-presentment-outside-fx`

Portal display FX is non-authoritative, computed outside rating-core, and MUST be labelled estimates
([`../PRD.md`](../PRD.md) §6.9). Nothing in the resolved outcome depends on a presentment amount.

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-domain-model-fx`

- **`CurrencyRoles`** — value object bound per line from the frozen context: `priceCurrency` (currency of the step-2 selected row), `billingCurrency` (payer account/contract invoice currency via the frozen Subscriptions binding); presentment is deliberately absent from the model.
- **`FxPolicyRecord`** — frozen Finance input: policy kind (`per_window_rate_lock` \| `invoice_period`), `fxTableVersion`, locked-rate id (rate-lock), and the rate material for the pair at `t`.
- **`FxApplication`** — the outcome fragment: pre-conversion amount (price currency), post-conversion amount (billing currency), policy kind, `fxTableVersion` / locked-rate id, and the **provisional** flag (invoice-period only).
- **`FxLockSegment`** — the S1 snapshot segment: FX-lock id (if any), written by Rating at evaluation (§4.3).
- **`FxCloseDelta`** — the close-time re-rate difference for provisional lines: same frozen tuple except the close-time `fxTableVersion`; leaves under the slice-08 correction key (§4.2).

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-component-conversion-fx`

- **`CurrencyRoleResolver`** — binds `CurrencyRoles` from the frozen context; short-circuits step 8 when billing = price (native multi-currency, §17.1 step 2). Authoritative: the slice-02 FX-skip flag is advisory metadata, re-derived here.
- **`FxPolicyApplier`** — applies the frozen table under the selected policy; produces `FxApplication`; fails closed on any missing table / policy / lock record.
- **`FxLockRecorder`** — hands the FX-lock segment to the `SnapshotComposer` before sealing; stamps `fxTableVersion` / locked-rate id into the outcome metadata (`MetadataRecorder`).
- **`BillingCurrencyCouponPass`** — re-invokes the slice-06 coupon evaluator for `settlementCurrency = billing` coupons on the converted amount, same `fxTableVersion` (§4.4).
- **`FxCloseDeltaCalculator`** — pure re-rate math (`FxCloseDelta`) invoked by the slice-08 wrapper at period close; owns no path of its own.

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-convert-fx`

The **step-8 evaluator contract** (conceptual; invoked by the Foundation pipeline):
`applyFx(lineAmount@priceCurrency, CurrencyRoles, FxPolicyRecord) → FxApplication`. Pure; same
inputs ⇒ byte-identical output (the `fxTableVersion` at each stage is part of the replay inputs,
[`../PRD.md`](../PRD.md) §6.9). Fail-closed problem values: missing FX record / policy / lock with
billing ≠ price; unbound billing currency (torn Subscriptions segment — rejected by the
`SnapshotComposer`, [`./01-foundation.md`](./01-foundation.md) §4.3).

The Finance FX input contract and the Billing handoff (provisional flag, rounding, close-time
authority) are owned by [`11-consumer-contracts.md`](./11-consumer-contracts.md); the close delta
leaves through the Foundation `cpt-cf-bss-rating-interface-reresolve-fnd` under slice 08.

### 3.4 Internal Dependencies

Foundation ([`01-foundation.md`](./01-foundation.md)): step-slot registration, determinism tuple,
`SnapshotComposer`, `EmissionGuard`. [`06-coupons.md`](./06-coupons.md): price-currency coupons
complete before this step; the billing-currency pass re-invokes the slice-06 evaluator — semantics
stay there, placement is here. [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md):
the invoice-period close delta is a correction under its keys.
[`09-period-plan-change.md`](./09-period-plan-change.md): floor/cap set in price currency converts
for comparison under the same policy + `fxTableVersion` (§17.2) — slice 09's obligation, this
slice's recorded policy identity. [`11-consumer-contracts.md`](./11-consumer-contracts.md): boundary contracts.

### 3.5 External Dependencies

| Dependency | What arrives frozen | Contract |
|------------|--------------------|----------|
| Finance | FX tables + lock policy, `fxTableVersion` / locked-rate ids | PRD §9.2 Finance FX; [`11-consumer-contracts.md`](./11-consumer-contracts.md) |
| Pricing (Product Catalog) | per-`(currency, region)` price rows (first-class, never FX-derived), ISO 4217 minor-unit amounts | PRD §9.2 read-model contract; pricing [`design/04`](../../../pricing/docs/design/04-currency-tax.md) |
| Subscriptions | `(currency, region)` binding frozen at activation | SEAMS S1; PRD §9.2 Subscriptions input |
| Promotions | frozen coupon snapshots incl. `settlementCurrency` | PRD §9.2 Promotions; [`06-coupons.md`](./06-coupons.md) |
| Billing | rounds in billing currency after conversion; period close fixes the authoritative close-time `fxTableVersion` | PRD §9.2 Billing; §17.1 rating-core/Billing boundary |

### 3.6 Interactions and Sequences

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-flow-step8-convert-fx`

**Step 8 within one line** (the step-8 leg of `cpt-cf-bss-rating-seq-evaluate-tariff`):

1. Resolve `CurrencyRoles`; if billing = price, skip conversion entirely (native multi-currency; no FX record required, no FX-lock segment written) and proceed to step 4 below.
2. Load the frozen `FxPolicyRecord`; missing with billing ≠ price ⇒ fail closed.
3. Convert at full precision: rate-lock ⇒ the locked rate, final at event time; invoice-period ⇒ the locked/spot rate from the frozen table, amount **flagged provisional**.
4. Apply `settlementCurrency = billing` coupons on the billing-currency amount via the slice-06 evaluator, same `fxTableVersion` (§4.4).
5. Record `fxTableVersion` / locked-rate id in metadata; `FxLockRecorder` writes the S1 segment; hand to step 9 (`EmissionGuard`).

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-close-delta-fx`

**Invoice-period FX delta at period close** (boundary with slice 08):

1. Billing closes the period; the close-time `fxTableVersion` becomes authoritative ([`../PRD.md`](../PRD.md) §6.9).
2. The slice-08 wrapper re-rates each provisional line via `FxCloseDeltaCalculator`: identical frozen tuple and pinned snapshot, only the close-time `fxTableVersion` substituted — a **full re-execution of step 8 plus the billing-currency coupon pass and the step-9 guards**, diffed at the line level. (A `percent` coupon re-scales with the converted amount; a billing-currency `fixed_amount` coupon does not — recomputing the conversion alone would mis-state the delta.)
3. The difference leaves as a delta keyed `(unitKey[, slice], prior-rated-version, snapshot)` via the Adjustment path ([`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md)); the provisional line is never mutated.
4. Replay of either stage is byte-identical given which `fxTableVersion` applied at which stage — both are recorded inputs.

### 3.7 Database Schemas and Tables

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-storage-none-fx`

**None owned.** FX tables and lock policies live in Finance; the recorded `fxTableVersion` /
locked-rate id and the FX-lock segment ride the emitted outcome (Rating persistence). The only local
state is a non-authoritative cache of frozen table pages keyed by `fxTableVersion`, whose loss
degrades latency, never correctness.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-fx`

Runs in the `rating` gear (rating-core crate) ([`./01-foundation.md`](./01-foundation.md)
§3.8) — no additional topology. FX table pages are pinned per version and safely cold-startable;
there is no shared mutable rate state and no cross-partition coordination.

## 4. Additional Context

### 4.1 Currency Role Separation (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-currency-roles-fx`

- **Price currency** — the currency of the `Price.amount` row selected at step 2. Per-market rows are first-class catalog rows authored per `(currency, region)` in the pricing gear; **no FX derivation ever** — a missing market row is absent, not derivable (pricing [`design/04`](../../../pricing/docs/design/04-currency-tax.md); `currencyFallbackPolicy` is a pricing Future).
- **Billing currency** — the invoice currency per payer account/contract, delivered via the Subscriptions-frozen `(currency, region)` binding (S1).
- **Presentment currency** — portal display FX; non-authoritative, outside rating-core, labelled estimates.
- Conversion applies **iff** billing ≠ price; equal currencies skip step 8 (native multi-currency, §17.1 step 2).
- Single-currency-per-invoice is a pricing publish-time guarantee (pricing Slice 4 `CurrencyBindingChecker`); Rating relies on it and never mixes currencies within one line.

### 4.2 FX Policy Semantics (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-fx-policy-fx`

- Exactly two deterministic policies, selected by the frozen Finance policy ([`../PRD.md`](../PRD.md) §6.9): **(a) per-window rate-lock** — the locked rate is final at event time; the conversion is definitive and the lock id is recorded; **(b) invoice-period FX** — provisional amount at the locked/spot rate on the hot path (flagged provisional), authoritative re-rate at the close-time `fxTableVersion`, emitted **as a delta** under the slice-08 correction keys (§3.6).
- `fxTableVersion` is part of the determinism tuple ([`./01-foundation.md`](./01-foundation.md) §4.2): replay over identical inputs — including which version applied at which stage — is byte-identical.
- Missing FX record (table, policy, or lock) with billing ≠ price ⇒ **fail closed**; no implicit or provider-default rate exists in the design.
- The frozen table MUST carry the exact **pair-direction** record (price → billing) for the conversion; rate inversion or cross-rate derivation from other pairs is a derivation Rating never performs — absence of the exact pair fails closed.
- The policy-binding scope (which payer/account/contract binds which policy) arrives frozen from Finance; its concrete field shape is closed in [`11-consumer-contracts.md`](./11-consumer-contracts.md), not here.

### 4.3 FX-Lock Snapshot Segment (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-fx-lock-segment-fx`

- The FX-lock id (if any) is a **Rating-written segment** of `pricingSnapshotRef`, appended at evaluation and sealed by the `SnapshotComposer` (SEAMS S1; [`./01-foundation.md`](./01-foundation.md) §4.3).
- Segment population: rate-lock ⇒ the lock id; invoice-period ⇒ no lock id — the applied `fxTableVersion` + provisional flag ride the outcome metadata; native (step 8 skipped) ⇒ segment empty.
- The sealed ref is immutable: the close-time re-rate never rewrites it — the close delta carries its own correction key and records the close-time `fxTableVersion` in its own metadata.

### 4.4 Ordering and Precision at the FX Boundary (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-normative-fx-ordering-fx`

- **Order**: coupons with `settlementCurrency = price` complete in step 7 **before** conversion; conversion runs once; coupons with `settlementCurrency = billing` apply **after** conversion on the billing-currency amount under the **same** `fxTableVersion`; then the step-9 guards. No other step converts ([`../PRD.md`](../PRD.md) §17.1 steps 7–8, §17.2; [`./01-foundation.md`](./01-foundation.md) §3.6).
- **Native-currency lines**: only the conversion is skipped — the billing-currency coupon pass still executes in its step-8 position on the (identical-currency) amount, keeping coupon placement invariant. The PRD states only the FX skip (§17.1 step 2); this placement reading is fixed here in Design and exercised by the slice-06 joint coupon fixture.
- **Precision**: conversion computes at full intermediate precision and never rounds; Billing rounds in billing currency after conversion, and the emission records the rounding-policy id ([`./01-foundation.md`](./01-foundation.md) §4.4). The non-negative guard runs at step 9, after the billing-currency coupon pass.
- **Period floor/cap**: amounts set in price currency convert for billing-currency comparison with the same FX policy + `fxTableVersion` as step 8 (§17.2) — executed under slice 09's obligation, with this slice's recorded policy identity.

## 5. Traceability

**Traces to**: `cpt-cf-bss-rating-fr-multi-currency`, `cpt-cf-bss-rating-fr-fx-policy`

- **PRD**: §6.9 (`fr-multi-currency`, `fr-fx-policy`), §17.1 step 8 + step-2 native skip + "Multi-currency (preserved)", §17.2 (billing-currency coupons; floor/cap currency), §12 AC 8, §4.1, §9.2 (Finance FX input contract), §7.1 NFRs.
- **Seams**: S1 (fx-lock segment — the Rating-written segment owned by this slice), W2 (the close delta replays the pinned snapshot) — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-03 (snapshot composition), T-D-04 (snapshot-only replay for the close delta) — [`../DECISIONS.md`](../DECISIONS.md).
- **Slices**: [`01-foundation.md`](./01-foundation.md) (pipeline slot, determinism tuple, emission guards), [`06-coupons.md`](./06-coupons.md) (coupon ordering across the FX boundary), [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) (close-time delta path), [`09-period-plan-change.md`](./09-period-plan-change.md) (floor/cap conversion), [`11-consumer-contracts.md`](./11-consumer-contracts.md) (Finance/Billing contracts).
- **Pricing design set**: [`04-currency-tax.md`](../../../pricing/docs/design/04-currency-tax.md) (per-market rows, no FX derivation, currency binding), [`06-consumer-contracts.md`](../../../pricing/docs/design/06-consumer-contracts.md) (frozen read-model consumer contract).
