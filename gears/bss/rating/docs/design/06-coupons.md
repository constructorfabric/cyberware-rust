Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Rating — Coupons (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Promotions (frozen coupon snapshots), Finance (fxTableVersion for the billing-currency pass) | Downstream: Rating, Billing/Tax (discount lineage) | Owners: BSS Rating team -->

# DESIGN — Coupons (Slice 6)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-design-coupons`

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
  - [4.1 Placement and the FX Split (normative)](#41-placement-and-the-fx-split-normative)
  - [4.2 Stacking Policies (normative)](#42-stacking-policies-normative)
  - [4.3 applyScope Attachment and Hybrid Split-Back (normative)](#43-applyscope-attachment-and-hybrid-split-back-normative)
  - [4.4 Frozen Coupon Snapshot and Fail-Closed Rules (normative)](#44-frozen-coupon-snapshot-and-fail-closed-rules-normative)
  - [4.5 Snapshot Segment and Discount Lineage (normative)](#45-snapshot-segment-and-discount-lineage-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice is the **step-7 evaluator**: coupons are the Promotions overlay applied to the
**post-commitment** resolved commercial price — after the overlay/contract/commitment steps 4–6
and split around FX at step 8 ([`../PRD.md`](../PRD.md) §6.8, §17.2 order chain). Rating owns
**application semantics only**: which of the frozen, already-redeemed coupon snapshots applies to
which line, in what order, with what recorded lineage. The coupon entity lifecycle — creation,
campaigns, distribution, redemption limits, fraud controls — is Promotions' and explicitly out of
Rating scope ([`../PRD.md`](../PRD.md) §5.2).

Two invariants shape the slice. First, **the ordering is compiled in**: price-currency coupons
apply at step 7 before FX; billing-currency coupons apply only after step 8, on the
billing-currency amount, under the same `fxTableVersion` ([`01-foundation.md`](./01-foundation.md)
§3.6). Second, **policy comes only from the frozen snapshot**: a snapshot missing `applyScope`
(or `stackSequence` under `ordered_stack`) fails closed — Rating never infers a coupon rule from
mutable campaign state, and replay uses the pinned snapshot, never live Promotions state
([`../PRD.md`](../PRD.md) §6.8, §9.2). The slice owns no scope-key seam; its cross-gear surface
is the S1 coupon segment of `pricingSnapshotRef` (§4.5).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-rating-fr-coupon-application-order` | A registered step-7 evaluator over the post-commitment amount plus a **deferred billing-currency pass** after step 8 (§4.1): `settlementCurrency = price` before FX, `= billing` after, same `fxTableVersion`; applied ids + pre-/post-discount amounts always recorded (§4.5). |
| `cpt-cf-bss-rating-fr-coupon-stacking` | `exclusive_best` default (single best coupon = lowest resulting charge) vs campaign-linked `ordered_stack` (ascending `stackSequence`, compounding); incompatible pairs fail closed; missing policy fields fail closed, never inferred (§4.2, §4.4). |
| `cpt-cf-bss-rating-fr-hybrid-pricing` | Per-`applyScope` attachment on hybrid plans: `usage`/`recurring` bind to that line; `line_total` applies once as a plan-scoped overlay and splits back pro-rata across the two lines at full precision (§4.3) — **deferred at launch, fails closed** for want of a plan-scoped evaluation base (T-D-22, §4.2). |
| `cpt-cf-bss-rating-fr-snapshot-carry` | The applied coupon id(s) + stacking policy are a **Rating-written segment** of `pricingSnapshotRef`, sealed by the `SnapshotComposer` at emission (§4.5; SEAMS S1). |
| `cpt-cf-bss-rating-fr-evaluation-order` | Step 7 is a fixed slot after commitment and before FX; the intra-step order (eligibility → currency partition → stacking → attachment → record) is equally compiled in (§3.6). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-rating-nfr-throughput-latency` | Step-7 evaluator | No Promotions call on the hot path — coupon snapshots arrive frozen in the context; both passes are in-process arithmetic | Load test; targets provisional ([`../PRD.md`](../PRD.md) §7.1) |
| `cpt-cf-bss-rating-nfr-horizontal-scale` | Partitioning contract | Stateless per-unit application; no shared redemption state is read or written at evaluation | Design + load test |
| `cpt-cf-bss-rating-nfr-resilience` | Fail-closed guards | Missing `applyScope` / `stackSequence` / unresolvable enum values fail closed — never an inferred discount ([`../PRD.md`](../PRD.md) §12 AC 16) | Retry + conformance fixtures |

#### Key ADRs

| ADR ID | Decision Summary |
|--------|------------------|
| `cpt-cf-bss-rating-adr-scope-key-adoption` | Not load-bearing at step 7 itself — coupons discount the already-resolved line; listed because the rows being discounted resolve on the adopted 8-axis key upstream (slice 02) and the coupon segment joins that same composed ref. |

No slice-local ADR: coupon semantics are PRD-normative ([`../PRD.md`](../PRD.md) §6.8, §17.2);
the segment ownership follows **T-D-03** and replay follows **T-D-04**
([`../DECISIONS.md`](../DECISIONS.md)).

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-tech-stack-cpn`

```text
Step-7 evaluator (this slice)   EligibilityFilter → currency partition → StackingResolver
        │  (fixed §17.1 slot 7)     (exclusive_best | ordered_stack) → ScopeAttacher
        │                        + deferred billing-currency pass (after step 8)
        ▼
Evaluation pipeline (01)        EvaluationPipeline · SnapshotComposer (coupon segment) ·
                                MetadataRecorder (discount lineage) · EmissionGuard
        │
        ▼
Frozen inputs (external SoRs)   coupon snapshots (Promotions) · post-step-6 line state ·
                                fxTableVersion (Finance via step 8, billing-currency pass)
```

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | The step-7 evaluator, both application passes, stacking/attachment rules | Rust module in the `rating` gear (rating-core crate), registered into the Foundation pipeline |
| Domain | Coupon-snapshot input shape, application/lineage value objects (§3.1) | Rust; GTS + Rust domain structs |
| Infrastructure | **None** — no store, no cache; snapshots arrive frozen in the `EvaluationContext` | n/a |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Apply, never own

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-principle-apply-never-own-cpn`

Rating applies frozen coupon snapshots and records the result; it never creates, expires,
counts, or invalidates a redemption — the coupon lifecycle is Promotions'
([`../PRD.md`](../PRD.md) §5.2). Nothing in this slice writes toward Promotions.

#### Policy from the snapshot only

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-principle-policy-from-snapshot-cpn`

Every application decision — scope, stacking, sequence, validity, settlement currency — is read
from the frozen snapshot fields; an absent policy field is a fail-closed error, never a default
guessed at evaluation time ([`../PRD.md`](../PRD.md) §6.8, §9.2 "never infers coupon rules from
mutable campaign UI state").

#### Lineage is part of the outcome

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-principle-lineage-first-cpn`

Pre-/post-coupon amounts and applied ids are not diagnostics — they are the contractual handoff
that lets Billing/Tax choose gross-vs-net treatment and lets audit reproduce the discount
([`../PRD.md`](../PRD.md) §5.2; [`01-foundation.md`](./01-foundation.md) §4.4). An application
without recorded lineage is an invalid outcome.

### 2.2 Constraints

#### Fixed slot, compiled FX split

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-constraint-fixed-slot-cpn`

Step 7 always runs after steps 4–6 on the post-commitment amount; the price-vs-billing currency
split around step 8 is part of the compiled §17.1 order — there is no placement or reordering
configuration ([`../PRD.md`](../PRD.md) §6.8, §17.1 steps 7–8).

#### No redemption mutation, snapshot-only replay

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-constraint-no-redemption-mutation-cpn`

Evaluation and re-resolution never read live Promotions state: corrections and replays reuse the
coupon snapshot pinned in `pricingSnapshotRef` (W2 / T-D-04), so a campaign edit or redemption
event after the pin can never change a replayed outcome.

#### Promotions contract maturity

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-constraint-promotions-maturity-cpn`

The Promotions PRD does not exist yet; [`../PRD.md`](../PRD.md) §17.2 is the **Rating-side
stub** and this slice's contract source. **Open** — field names and the coupon-snapshot event
contract MUST be aligned with Promotions before production coupon rating
([`../PRD.md`](../PRD.md) §15, §16).

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-domain-model-cpn`

- **`CouponSnapshot`** — the frozen input, minimum fields per [`../PRD.md`](../PRD.md) §17.2: `couponId`, `adjustmentType` (`percent | fixed_amount`), `value`, `settlementCurrency` (`price | billing`), `applyPerTierBand`, `applyScope` (`usage | recurring | line_total`), `stackSequence` (required under `ordered_stack`), validity, applicability filters, redemption eligibility.
- **`StackingPolicy`** — `exclusive_best | ordered_stack`; carried by the snapshot/campaign link, recorded into the coupon segment (§4.5).
- **`CouponApplication`** — one applied coupon on one line: id, pass (`price | billing` currency), basis amount, discount amount, resulting amount.
- **`DiscountLineage`** — per line and per pass: pre-/post-coupon amounts, applied ids, stacking policy, and the `line_total` split-back shares on hybrid plans (§4.3).

### 3.2 Component Model

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-component-coupon-evaluator-cpn`

- **`Step7Evaluator`** — the registered evaluator; runs the price-currency pass at step 7 and provides the deferred billing-currency pass invoked after step 8; enforces the fail-closed guards.
- **`EligibilityFilter`** — evaluates validity at `t`, applicability filters, and redemption eligibility strictly against the frozen snapshot fields.
- **`StackingResolver`** — `exclusive_best`: selects the single coupon yielding the lowest resulting charge; `ordered_stack`: folds ascending `stackSequence`, each step applying to the prior output (§4.2).
- **`ScopeAttacher`** — routes per `applyScope` to the usage line, the recurring line, or the plan-scoped `line_total` with deterministic split-back; routes `applyPerTierBand` to band-level vs total-after-tier-math application (§4.3).

Applied ids + stacking policy reach the composed ref via the Foundation `SnapshotComposer`;
lineage rides the outcome envelope via the `MetadataRecorder`
([`01-foundation.md`](./01-foundation.md) §3.2).

### 3.3 API Contracts

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-interface-step7-evaluator-cpn`

The **step-evaluator contract** (in-process, registered): input = post-step-6 line state + the
frozen coupon snapshot set from the context; output = `CouponApplication[]` + `DiscountLineage`,
applied to the line amount. A second application point — the **billing-currency pass** — is
invoked by the pipeline after step 8 with the converted amount and the step-8 `fxTableVersion`.
Fail-closed problem values: missing `applyScope`; missing `stackSequence` under `ordered_stack`;
an unrecognized `adjustmentType` / `settlementCurrency` value; an incompatible pair arriving
both-applicable (§4.2); an `exclusive_best` candidate set whose benefit comparison is undefined
(§4.2 open). External boundary contracts are owned by
[`11-consumer-contracts.md`](./11-consumer-contracts.md).

### 3.4 Internal Dependencies

Depends on [`01-foundation.md`](./01-foundation.md) (slot registration, `SnapshotComposer`,
`MetadataRecorder`, `EmissionGuard`, determinism keys), on
[`05-commitments-reservations.md`](./05-commitments-reservations.md) (the post-commitment amount
is the step-7 basis), on [`03-metering-models.md`](./03-metering-models.md) (the graduated
outcome exposes marginal-band amounts for `applyPerTierBand = true`), and on
[`04-overlays-precedence.md`](./04-overlays-precedence.md) (coupon and partner discount both
apply — partner in step 4, coupon in step 7; never mutually exclusive,
[`../PRD.md`](../PRD.md) §17.2). Downstream: [`07-currency-fx.md`](./07-currency-fx.md) hosts
the step-8 conversion the billing-currency pass follows;
[`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) replays the pinned coupon
snapshot; [`09-period-plan-change.md`](./09-period-plan-change.md) owns the floor/cap boundary
the §15 clawback open belongs to.

### 3.5 External Dependencies

| Dependency | What arrives frozen | Contract |
|------------|--------------------|----------|
| Promotions | Frozen coupon snapshots (field list [`../PRD.md`](../PRD.md) §17.2); lifecycle, campaigns, redemption limits stay Promotions-side | [`../PRD.md`](../PRD.md) §9.2 coupon snapshot contract; fail-closed compatibility |
| Finance (via step 8) | The `fxTableVersion` / locked-rate id the billing-currency pass reuses | [`../PRD.md`](../PRD.md) §6.8, §6.9 |
| Billing / Tax | Consume discount lineage for gross-vs-net treatment; Rating supplies lineage, not ordering | [`../PRD.md`](../PRD.md) §5.2 |

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-step7-line-cpn`

**Step 7 on one line — price-currency pass** (within `cpt-cf-bss-rating-seq-evaluate-tariff`):

1. Filter the frozen coupon set: validity at `t`, applicability filters, redemption eligibility, and complete policy fields — any absent policy field fails closed (§4.4).
2. Partition candidates by `settlementCurrency`: the `price` set proceeds now; the `billing` set is deferred to the post-step-8 pass.
3. Resolve stacking (§4.2): `exclusive_best` selects the single lowest-resulting-charge coupon; `ordered_stack` folds ascending `stackSequence` over the prior output.
4. Attach per `applyScope` (§4.3): `usage`/`recurring` to that line; `line_total` once, plan-scoped, split back pro-rata at full precision; `applyPerTierBand` routes band-level application.
5. Record: applied ids + stacking policy into the coupon segment; pre-/post-discount amounts into lineage; the line proceeds to step 8.

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-flow-billing-currency-pass-cpn`

**Billing-currency pass** (after step 8):

1. Apply the deferred `settlementCurrency = billing` candidates to the billing-currency amount produced by step 8, under the **same `fxTableVersion`** ([`../PRD.md`](../PRD.md) §6.8, §17.1 step 8).
2. Stacking, attachment, and recording rules are identical to the price-currency pass; both passes complete before the step-9 `EmissionGuard`, whose non-negative rule covers the post-coupon amount ([`../PRD.md`](../PRD.md) §17.1 step 9).

### 3.7 Database Schemas and Tables

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-storage-none-cpn`

**None owned.** Coupon entities, campaigns, and redemption state live in Promotions; the applied
result persists only inside the rated outcome (segment + lineage) owned by Rating; there is no
coupon cache — the snapshot set arrives in the context
([`01-foundation.md`](./01-foundation.md) §3.7).

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-rating-deployment-cpn`

A stateless registered evaluator in the `rating` gear (rating-core crate) with
two in-process application points (step 7 and the post-step-8 hook) — identical topology to the
Foundation ([`01-foundation.md`](./01-foundation.md) §3.8); nothing slice-specific beyond
registration.

## 4. Additional Context

### 4.1 Placement and the FX Split (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-placement-fx-split-cpn`

- Coupons apply at **step 7**, strictly after steps 4–6, on the **post-commitment** line amount; the normative chain is Catalog base → PriceOverlays → Customer/contract → Commitment → **Coupon** → FX → Emit ([`../PRD.md`](../PRD.md) §6.8, §17.2).
- `settlementCurrency = price` coupons apply at step 7 in **price currency, before FX**; `settlementCurrency = billing` coupons apply **only after step 8**, on the billing-currency amount, under the **same `fxTableVersion`** ([`../PRD.md`](../PRD.md) §6.8, §12 AC 16). The split is compiled into the pipeline ([`01-foundation.md`](./01-foundation.md) §3.6) — no knob.
- Native multi-currency: when invoice currency equals the row's price currency, step 8 is skipped ([`../PRD.md`](../PRD.md) §17.1 step 2); the two passes then operate on the same currency but keep their fixed relative order — price-currency set first, billing-currency set at the (skipped) FX point.
- Coupon and partner discount **both** apply (partner in step 4, coupon in step 7); overlay stacking never excludes a coupon and vice versa ([`../PRD.md`](../PRD.md) §17.2).

### 4.2 Stacking Policies (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-stacking-cpn`

- **`exclusive_best`** (default): exactly one coupon applies per line — the one yielding the largest customer benefit, i.e. the **lowest resulting charge**; all others MUST NOT apply on the same line ([`../PRD.md`](../PRD.md) §6.8, §12 AC 16). **Cross-scope exclusivity (T-D-20, 2026-07-28 review #8):** when the candidate set contains a `line_total`-scoped coupon, the exclusivity set **widens to the plan**: exactly one coupon applies **per plan per period** — the winner is the single candidate with the greatest total benefit, each measured on its own scope (a `usage`/`recurring` candidate's benefit on its line; a `line_total` candidate's on the combined plan total), amounts at full intermediate precision in price currency. Without this rule a usage-scoped and a line_total-scoped coupon are "not on the same line" and both apply — double discounting under the default policy. With **no** `line_total` candidate, exclusivity stays per line (two lines of a hybrid can each carry their own winner). **At launch T-D-20 is inert** — no `line_total` candidate can enter the set (T-D-22, below); the rule is written normatively so it lands with the plan-scoped base rather than being re-derived later.
- **Open — equal-benefit tie-break**: the PRD does not pin the order between two coupons producing an identical resulting charge; determinism requires a total order. Default proposal: ascending `couponId` as the stable tie-break — applying identically to the T-D-20 cross-scope comparison (a line-scoped and a plan-scoped candidate with equal total benefit tie-break on `couponId` too); set with Promotions before production coupon rating ([`../PRD.md`](../PRD.md) §15 Promotions row).
- **Open — mixed-settlement comparison**: when an `exclusive_best` candidate set mixes `settlementCurrency` values, the benefit-comparison basis across the FX boundary is an absent policy — evaluation fails closed until it is pinned with Promotions/Finance ([`../PRD.md`](../PRD.md) §15); never a guessed cross-currency comparison.
- **Open — plan-scoped evaluation base (T-D-22, 2026-07-28 review; `line_total` fails closed at launch)**: step 7 runs **per line** (§4.2 sequence heading) and every synthesized evaluation unit is **sub-plan** — `per_event`, a windowed-`Q` sub-window slice, or a period-driven unit keyed `(subscription, priceId, chargeKind, lineKey, AnchorPeriod)` (slice [`14`](./14-unit-synthesis-period-tick.md) §4.1, T-D-15/T-D-18). On a hybrid plan the usage line and the recurring line are therefore **distinct units fired by distinct triggers** (usage arrival / window close vs the period tick), so **no point in the pipeline holds both pre-coupon amounts at once** — which is exactly what a `line_total` coupon needs (apply once plan-scoped, split back pro-rata — §4.3) and what T-D-20 needs (compare a line-scoped candidate's benefit against a plan-total candidate's). Until that base is defined, a `line_total`-scoped coupon snapshot **fails closed** at evaluation: it is never applied to a single line, never split against a partially-rated plan, and never silently downgraded to `usage`/`recurring` scope. Pinning it means naming the **plan-period assembly point** (which component holds the plan's line set for an `AnchorPeriod`), its **trigger** (it cannot precede the last member line's rating, yet an open period keeps re-resolving usage), and its **cascade** behaviour (a member line re-rating must re-drive the plan-scoped discount and its split-back as deltas under the standard correction key) — a named joint gate with Promotions, alongside the tie-break and mixed-settlement opens above ([`../DECISIONS.md`](../DECISIONS.md) T-D-22; [`../SEAMS.md`](../SEAMS.md) §Decisions register).
- **`ordered_stack`**: applies only when a Promotions campaign explicitly links coupons with `stackSequence`; application folds in ascending sequence, each step using the prior step's output (compounding); a missing `stackSequence` under `ordered_stack` fails closed ([`../PRD.md`](../PRD.md) §6.8) — and so does a **duplicate** `stackSequence` within one linked set (2026-07-31 review, #28): the fold is order-dependent the moment types mix (on 100, a 20% and a $10 coupon give 72 one way and 70 the other), so an unordered pair under an *ordered* policy is exactly the non-determinism the byte-identical rule (§4.4) forbids — fail closed, never a tie-break (the equal-benefit tie-break above orders *comparisons*, not a fold the campaign author left ambiguous).
- **Open — mixed-settlement `ordered_stack`**: a linked sequence mixing `settlementCurrency` values cannot fold in sequence order — the compiled two-pass placement (price-currency at step 7, billing-currency after step 8) would contradict the `stackSequence`. Such a campaign **fails closed** until the rule is pinned with Promotions (the mirror of the `exclusive_best` mixed-settlement open); the candidate rule — fold within each pass, sequence preserved per pass — goes to the same workshop.
- **Incompatible pairs** fail closed at **redemption bind time** (Promotions-side); if an incompatible pair nevertheless arrives both-applicable on one line, the bind invariant is violated and evaluation fails closed — never a silent pick ([`../PRD.md`](../PRD.md) §6.8).

### 4.3 applyScope Attachment and Hybrid Split-Back (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-applyscope-cpn`

- `applyScope ∈ {usage, recurring, line_total}`. The documented default `line_total` is an **authoring-side** default Promotions writes into the snapshot ([`../PRD.md`](../PRD.md) §17.2); a snapshot *arriving* without `applyScope` still fails closed — Rating never assumes the default (`fr-coupon-stacking`). **Launch (T-D-22):** only `usage` and `recurring` are evaluable; an arriving `line_total` fails closed for want of a plan-scoped base (§4.2). Since `line_total` is also the *authoring* default, Promotions MUST write an explicit line scope for every launch coupon — the joint gate covers this alongside the assembly point.
- On a hybrid plan ([`../PRD.md`](../PRD.md) §6.2): `usage`/`recurring` binds the coupon to that emitted line; `line_total` applies **once** to the combined total as a plan-scoped overlay and is split back **pro-rata to the two lines' pre-coupon amounts** — deterministic and exact because the split runs at full intermediate precision (no rounding in rating-core, [`01-foundation.md`](./01-foundation.md) §4.4). **The split-back rules below are specified but not reachable at launch** — they presuppose the plan-scoped assembly point that T-D-22 defers, so a `line_total` coupon fails closed before reaching them (§4.2). **Degenerate branch (2026-07-28 review #20):** when both pre-coupon amounts are 0 (zero-usage period + a `prepaid_drawdown` zero-due recurring line) the pro-rata weights are undefined — the whole (necessarily zero) discount is assigned to the **recurring** line deterministically, so lineage and replay agree across implementations.
- **`applyPerTierBand`**: default (false) applies the discount to the **total line amount after tier math** (a 10% coupon on a graduated total of 100 discounts 10); `true` applies per **marginal band**, consuming the band-level amounts exposed by [`03-metering-models.md`](./03-metering-models.md) ([`../PRD.md`](../PRD.md) §17.2, §12 AC 16). On a line without bands (`flat`/`per_unit`/`package`), `applyPerTierBand = true` degrades to the total-amount application (numerically identical) and is recorded as applied-to-total in lineage.

### 4.4 Frozen Coupon Snapshot and Fail-Closed Rules (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-fail-closed-snapshot-cpn`

- The consumption contract (Rating ← Promotions) is the frozen snapshot with at minimum: `couponId`, `adjustmentType` (`percent | fixed_amount`), `value`, **`valueCurrency` (the `fixed_amount` value's denomination — an explicit frozen field, required for `fixed_amount`; 2026-07-31 review, #31: the §fail-closed denomination guard below compared against a field no list carried — `settlementCurrency` only selects which pass runs, so without `valueCurrency` the guard was vacuous; the slice-05 spend-pool analogue names denomination coverage an authoring obligation the same way)**, `settlementCurrency` (`price | billing`), `applyPerTierBand`, `applyScope`, `stackSequence` (required under `ordered_stack`, **unique within the linked set** — §4.2), validity, applicability filters, redemption eligibility ([`../PRD.md`](../PRD.md) §17.2, §9.2).
- Fail-closed set (never inferred, never defaulted at evaluation): missing `applyScope`; missing **or duplicate** `stackSequence` under `ordered_stack` (#28); missing `valueCurrency` on a `fixed_amount` coupon (#31); unrecognized `adjustmentType` / `settlementCurrency` values; a `fixed_amount` coupon whose `valueCurrency` does not match its pass (price-currency pass ⇒ price currency; billing-currency pass ⇒ billing currency — the denomination is a frozen snapshot field, never converted by Rating); the §4.2 opens (mixed-settlement comparison; mixed-settlement `ordered_stack`) until pinned; and **`applyScope = line_total`** until the plan-scoped evaluation base is pinned (T-D-22, §4.2).
- Eligibility (validity window at `t`, applicability filters, redemption eligibility) is evaluated **against the frozen snapshot fields only**; expiry, redemption counting, and fraud controls remain Promotions-side lifecycle ([`../PRD.md`](../PRD.md) §5.2).
- **Determinism**: the same frozen coupon snapshot + the same inputs ⇒ byte-identical outcome ([`01-foundation.md`](./01-foundation.md) §4.2); open-period and posted-period corrections replay the coupon snapshot pinned in `pricingSnapshotRef` (W2 / T-D-04) — a Promotions-side change after the pin never alters a replay.

### 4.5 Snapshot Segment and Discount Lineage (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-normative-segment-lineage-cpn`

- **S1 segment**: `applied coupon id(s) + stacking policy` is the Rating-written-at-evaluation segment of `pricingSnapshotRef` owned by this slice, sealed by the `SnapshotComposer` at emission and immutable thereafter ([`../SEAMS.md`](../SEAMS.md) D/S1; [`01-foundation.md`](./01-foundation.md) §4.3).
- **Lineage**: pre-/post-coupon amounts and applied ids — per line and per pass (price- and billing-currency) — ride the outcome envelope via the `MetadataRecorder` for Billing/Tax gross-vs-net treatment and audit ([`../PRD.md`](../PRD.md) §5.2, §6.8; [`01-foundation.md`](./01-foundation.md) §4.4). Rating supplies lineage, not the gross-vs-net ordering decision.
- **Floor interplay** (boundary, not owned here): whether a contractual period floor claws back coupon discount is a [`../PRD.md`](../PRD.md) §15 open (default proposal: the floor compares the post-coupon total); the floor/cap boundary is [`09-period-plan-change.md`](./09-period-plan-change.md)'s — this slice guarantees the lineage that keeps either answer computable.

## 5. Traceability

**Traces to**: `cpt-cf-bss-rating-fr-coupon-application-order`, `cpt-cf-bss-rating-fr-coupon-stacking`

- **PRD**: §6.8 (`fr-coupon-application-order`, `fr-coupon-stacking`), §6.2 (`fr-hybrid-pricing` applyScope attachment), §6.1 (`fr-snapshot-carry`), §6.3 `fr-evaluation-order`, §17.1 steps 7–8, §17.2 (coupon boundary contract — field list + order chain), §9.2 (Promotions coupon snapshot contract), §5.2 (lifecycle exclusions), §15 (Promotions contract alignment; floor-clawback open), §12 AC 16 "Coupon application order and stacking" (the vendored AC-numbering drift was fixed in the PRD 2026-07-28 — the rationales now cite AC 16).
- **Seams**: no scope-key seam owned; S1 coupon segment (§4.5) — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: T-D-03 (one ref, Rating composition SoR), T-D-04 (snapshot-only replay) — [`../DECISIONS.md`](../DECISIONS.md).
- **ADR**: [`../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md`](../ADR/0001-cpt-cf-bss-rating-adr-scope-key-adoption.md) (context only — §1.2).
- **Step slices**: [`01-foundation.md`](./01-foundation.md) (pipeline, composer, recorder, guards); [`03-metering-models.md`](./03-metering-models.md) (band-level basis for `applyPerTierBand`); [`04-overlays-precedence.md`](./04-overlays-precedence.md) (partner discount coexistence); [`05-commitments-reservations.md`](./05-commitments-reservations.md) (post-commitment basis); [`07-currency-fx.md`](./07-currency-fx.md) (step-8 conversion, `fxTableVersion`); [`08-retroactivity-corrections.md`](./08-retroactivity-corrections.md) (pinned-snapshot replay); [`09-period-plan-change.md`](./09-period-plan-change.md) (floor/cap boundary); [`11-consumer-contracts.md`](./11-consumer-contracts.md) (Promotions boundary contract).
