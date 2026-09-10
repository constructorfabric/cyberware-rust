---
refs:
  - bss/manifest/vz-arch-manifest-bss-only.md
  - bss/prd/PRD-billing-ledger-balances-202604041200/PRD-billing-ledger-balances-202604041200.md
  - bss/prd/PRD-contracts-agreements-202601120119/PRD-contracts-agreements-202601120119.md
  - bss/prd/PRD-metering-pricing-module-202601120119/PRD-metering-pricing-module-202601120119.md
  - bss/prd/PRD-product-catalog-marketplace-202601120119/PRD-product-catalog-marketplace-202601120119.md
  - bss/prd/PRD-rating-engine-202604031200/PRD-rating-engine-202604031200.md
  - bss/prd/PRD-subscriptions-lifecycle-202604021200/PRD-subscriptions-lifecycle-202604021200.md
---

Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

# PRD — Rating — Usage Rating & Commercial Pricing Logic

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Architecture Alignment](#2-architecture-alignment)
  - [2.1 Terminology and Naming](#21-terminology-and-naming)
  - [2.2 Predecessor PRDs and Scope Migration](#22-predecessor-prds-and-scope-migration)
- [3. Actors](#3-actors)
  - [3.1 Human Actors](#31-human-actors)
  - [3.2 System Actors](#32-system-actors)
- [4. Operational Concept & Environment](#4-operational-concept--environment)
  - [4.1 Module-Specific Environment Constraints](#41-module-specific-environment-constraints)
- [5. Scope](#5-scope)
  - [5.1 In Scope](#51-in-scope)
  - [5.2 Out of Scope](#52-out-of-scope)
- [6. Functional Requirements](#6-functional-requirements)
  - [6.1 Deterministic Evaluation](#61-deterministic-evaluation)
  - [6.2 Pricing Models](#62-pricing-models)
  - [6.3 Rule Evaluation Order](#63-rule-evaluation-order)
  - [6.4 Override Hierarchy and Overlays](#64-override-hierarchy-and-overlays)
  - [6.5 Tier Aggregation, Eligibility, Phases, Granularity](#65-tier-aggregation-eligibility-phases-granularity)
  - [6.6 Commitments and Reservations](#66-commitments-and-reservations)
  - [6.7 Dimensional (Cloud) Pricing](#67-dimensional-cloud-pricing)
  - [6.8 Coupons (Promotions Overlay)](#68-coupons-promotions-overlay)
  - [6.9 Multi-Currency and FX](#69-multi-currency-and-fx)
  - [6.10 Retroactivity and Corrections](#610-retroactivity-and-corrections)
  - [6.11 Period-Level and Plan-Change Obligations](#611-period-level-and-plan-change-obligations)
  - [6.12 Governance and ASC 606 Traceability](#612-governance-and-asc-606-traceability)
- [7. Non-Functional Requirements](#7-non-functional-requirements)
  - [7.1 NFR Inclusions](#71-nfr-inclusions)
  - [7.2 NFR Exclusions](#72-nfr-exclusions)
- [8. Five Quality Vectors Analysis](#8-five-quality-vectors-analysis)
- [9. Public Library Interfaces](#9-public-library-interfaces)
  - [9.1 Public API Surface](#91-public-api-surface)
  - [9.2 External Integration Contracts](#92-external-integration-contracts)
- [10. Use Cases](#10-use-cases)
- [11. User Interaction and Design](#11-user-interaction-and-design)
- [12. Acceptance Criteria](#12-acceptance-criteria)
  - [Price resolution and determinism](#price-resolution-and-determinism)
  - [Pricing models](#pricing-models)
  - [Time, versioning, currency](#time-versioning-currency)
  - [Retroactivity and corrections](#retroactivity-and-corrections)
  - [ASC 606 traceability](#asc-606-traceability)
  - [Tier aggregation, overlays, and eligibility](#tier-aggregation-overlays-and-eligibility)
  - [Promotions and coupons](#promotions-and-coupons)
  - [Plan change and proration](#plan-change-and-proration)
  - [Cloud resource pricing](#cloud-resource-pricing)
  - [Non-Functional Requirements (Show-Stoppers)](#non-functional-requirements-show-stoppers)
- [13. Dependencies](#13-dependencies)
- [14. Assumptions](#14-assumptions)
- [15. Open Questions](#15-open-questions)
- [16. Risks](#16-risks)
- [17. Reference Materials](#17-reference-materials)
  - [17.1 Rule Evaluation Order (normative appendix, steps 1-9)](#171-rule-evaluation-order-normative-appendix-steps-1-9)
  - [17.2 Boundary Contracts (coupons, floor/cap, plan-change proration)](#172-boundary-contracts-coupons-floorcap-plan-change-proration)
  - [17.3 Cloud Catalog Readiness and Phasing](#173-cloud-catalog-readiness-and-phasing)
  - [17.4 Future Scope](#174-future-scope)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

**Rating** is the BSS gear that turns metered usage and subscription state into **deterministic, auditable charges**. Per [ADR-0002](./ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md) it consists of two parts in one deployable: the **evaluation core** (`rating-core`) — a pure function that resolves **effective commercial prices and charge formulas** for subscriptions and usage meters in a **multi-tenant hierarchy** (platform owner → channel partner / reseller → end customer), under **usage-based and hybrid commercial models**, with **financial-grade auditability** and **byte-for-byte reproducible** outputs — and the **operational pipeline** (usage ingestion, windowed `Q` aggregation, dedup, evaluation-unit synthesis, rated-output persistence, handoffs — design slices 12–16). **Evaluation** (§6.3 / §17.1) emits a **resolved price outcome** plus a `pricingSnapshotRef`.

This gear owns the **evaluation semantics and the rating pipeline — not the price definitions**: tariffs (rate cards, pricing models, price windows, price overlays, effective dating, publish controls) are authored and owned by the **Pricing (Product Catalog)** gear and consumed here frozen (*adopt, don't fork* — [SEAMS](./SEAMS.md)). It does **not** compute tax, recognize revenue, manage coupon lifecycle, or enforce spend — those remain in their owning domains (§5.2).

### 1.2 Background / Problem Statement

BSS must monetize a real IaaS catalog (S3, VM, Disks) across a partner/reseller hierarchy with usage-based, hybrid, and committed-usage models. Without explicit, normative pricing semantics, rating batches, replay, and late-arrival handling diverge, and partner/customer override precedence becomes ad hoc — producing non-reproducible charges, disputes, and non-auditable financials.

This PRD fixes the **formula semantics** (flat, per_unit, tiered, volume, package, hybrid, committed-usage), the **deterministic evaluation order**, the **multi-currency** separation (price vs billing currency vs FX policy), and **CFO-grade controls** (rule-version audit, UTC effective dating, segregation of duties on publish, ASC 606-compatible tagging) so Design implements — not invents — these rules.

Industry alignment: usage-based pricing platforms (Metronome, Lago, OpenMeter) are the coverage/sequencing benchmark for cloud model breadth (dimensional, composite, capacity/reservation); partner economics require explicit precedence integers on price overlays; retroactivity never mutates posted invoices and uses delta adjustments with lineage (manifest §4.2).

### 1.3 Goals (Business Outcomes)

- **Determinism**: given frozen inputs `(window-aggregated inputs, pricingSnapshotRef, fxTableVersion)`, the monetary outcome is identical across replay, recompute, and cross-region batch workers; all divergences without input change are defects.
- **Model coverage**: flat, per_unit (per-seat), tiered (graduated), volume (Variant A), package (block), hybrid (recurring + usage), and committed-usage (drawdown + overage + true-up) are supported with explicit, configured semantics.
- **Multi-currency correctness**: price currency, invoice/settlement currency, and FX policy (rate-lock per window or invoice-period FX) are separated; no implicit provider-default FX.
- **Auditability**: rule-version audit trail, UTC effective dating, two-person rule on material publish, and ASC 606-compatible allocation inputs (PO tags, SSP pointers) carried as references — not a recognition engine.
- **Scale**: horizontal evaluation per tenant/partition with bounded p95 latency and no cross-partition locks on the hot path (working-assumption targets in §7.1 until NFR workshop).

### 1.4 Glossary

| **Term** | **Definition** |
|----------|----------------|
| **Tariff** | A versioned commercial rule set binding metering dimensions to a **pricing model** and **evaluation policy** for a Plan/Price row or overlay. Persisted via Catalog + contract overlays + snapshot refs (see Design for entity mapping). The word names **Pricing-owned rate definitions**; since ADR-0002 it no longer names this gear. |
| **Resolved price outcome** | The output of one **evaluation**: effective rates, pricing model kind, tier thresholds, overlay winners, and snapshot identifiers — not a separate Catalog entity. |
| **Evaluation context** | Inputs to resolve one price outcome: tenant axes (`resourceTenantId`, `payerTenantId`, `sellerTenantId`), subscription/plan linkage, **subscription phase**, **`planTier`**, SKU/meter, quantity or time slice (after **billing granularity** normalization), **`tierAggregationWindow`** policy, timestamp `t` (UTC), currency/region/brand scope, **`periodState`** (`open` \| `closed_posted`, from Billing), optional **`reservationMatch`**, optional **`changeEffectiveAt` / `changeMode`**, and applicable **snapshot identifiers**. |
| **periodState** | Open/closed state of the billing period covering `t`, **supplied by Billing**. `open` → retroactive/late events may re-resolve the window and FX may be provisional; `closed_posted` → posted-period immutability applies and corrections MUST be delta-only. Required input for the retroactivity branches. |
| **reservationMatch** | Optional input describing reserved/provisioned capacity at `t`: reserved rate, reserved/allocated quantity (`reservedQuantity`), and an optional usage-coverage flag. Two charge flavors: **(a) consumption-flavor** (matched usage at reserved rate, remainder on-demand); **(b) capacity-flavor** (allocated quantity charged at reserved rate regardless of usage). Entitlement lifecycle/inventory is cross-PRD (OSS/Contracts). |
| **capacityCharge** | The capacity-flavor charge: a recurring-style charge on `reservedQuantity` (e.g. provisioned-disk GB, provisioned IOPS) at the reserved rate, emitted per period independent of usage; evaluated at step 6. |
| **Tier aggregation window** | Policy governing when tier counter `Q` resets for tiered/volume models: `calendar_month`, `invoice_period`, `subscription_lifetime`, `per_event`, or `per_hour` (pricing D-313). MUST be configured on the Price/plan policy and frozen in `pricingSnapshotRef`. `calendar_month` delimited in UTC; `invoice_period` anchored to the subscription billing anchor per catalog `billingAnchorPolicy` (UTC; D-20 no-drift clamp); **`per_hour` delimited in UTC clock hours** — the same boundary the `hour` granule of `aggregationGranularity` already cuts on, so a level fold and an hourly counter agree on where an hour ends, and neither is anchored to the subscription (pricing D-313). Thresholds are half-open `[lower, upper)` — a quantity at a boundary falls in the UPPER band. Intra-window boundaries (mid-cycle activation, plan change, phase conversion) do NOT reset the counter by themselves: each sub-window slice prices its own attributed quantity with a **band offset** equal to the accumulated prior-slice `Q` (tier-counter continuity per pricing `inst-tb-window-continuity`); only a plan-change boundary may reset via the target plan's frozen `usageCounterOnPlanChange` (pricing D-113 / T-D-29; §6.11, §17.2). |
| **Billing granularity** | Minimum billable unit for a usage price (`per_second`, `per_minute`, `per_hour`, `per_day`, or whole-unit). Usage duration/quantity rounded **up** to this unit before rate application. A per-resource `minimumCharge` MAY bound ephemeral-resource over-charge (§15). |
| **dimensionKey** | Ordered tuple of pricing-relevant event dimensions that, with `meter`, identifies one charge line. Empty tuple = a meter with no declared dimensions. Invariant: one line per **`(meter, dimensionKey)`**. Declared dimensions come from the published Plan/SKU revision and are frozen in `pricingSnapshotRef`. Value emission on usage is an OSS/Rating contract. |
| **prorationBasis** | Day-count convention for mid-period proration: `calendar_days_actual`, `calendar_days_30`, `by_second`, `whole_unit`, or `none` (canonical enum owned by pricing `design/06`, adopted **verbatim** — CI gate `pricing.contracts.enum_drift`). Configured on the plan/price policy and frozen in `pricingSnapshotRef`; applies to ALL mid-period proration. |
| **Price eligibility** | Who may receive a `Price`/`PriceWindow`: `all_subscriptions`, `new_subscriptions_only`, or `existing_grandfathered`. Evaluated at step 2 with subscription `activatedAt` / grandfather cutover dates; within `existing_grandfathered` the **generation** is selected by the `cohort` of the subscription's pinned price id in `pricingSnapshotRef` (pricing `ADR/0002`), not `activatedAt` alone. |
| **Plan phase** | Time-bounded segment of a subscription plan (trial, intro, evergreen) with its own price schedule. Structure in Subscriptions SoR; evaluation resolves the active phase at `t` in step 1 to a **`phase_id` (uuid)** — the axis is uuid-typed (pricing D-19), never a kind-name; non-phased / one-time rows ride the plan's implicit **terminal `phase_id`**, and the kind names (trial/intro/evergreen) are display only. |
| **CatalogVersion** | Immutable, published revision of the product catalog (Catalog SoR). One component of a pricing snapshot. |
| **pricingSnapshotRef** | Immutable **composite** reference to all frozen commercial inputs needed to reproduce a charge. Canonical field list (per-segment writer): `catalogVersion` (pricing, pending→committed on `CatalogVersionPublished`) · resolved **price ids** incl. `cohort` (pricing) · evaluation-policy version (pricing) · `(currency, region)` binding (Subscriptions at activation) · resolved overlay/`priceOverlay` ids (Rating) · applied coupon id(s) + stacking policy (Rating) · FX-lock id if any (Rating) · commitment/reservation set — reservation match, pool set (incl. `poolType`, balances @ `balanceVersion`, draw order, rollover), reserved-vs-pool split (Rating). Rating is the **composition SoR** (assembles the full ref at eval). **Not** equivalent to `CatalogVersion` alone. |
| **PlanTier** | Mandatory catalog attribute on every Plan/SKU. Part of evaluation context; distinct from **OrgTier** (partner commercial projection). Primary mechanism for service-tier packaging (Basic/Pro/Enterprise) in current scope. |
| **OrgTier overlay** | Partner/reseller commercial projection applied without changing AMS tenant topology (manifest §4.1). |
| **Committed usage** | A committed quantity or spend pool (**commitment pool**, Contracts SoR) drawn down by metered usage; overage and true-up follow committed/overage rates. Two pool flavors (frozen `poolType`): `prepaid_drawdown` — the pool is billed upfront at sale (outside rating-core) and in-commit consumption is due-zero with notional lineage; `committed_rate` — in-commit consumption bills in arrears at the committed rate, with a period-end shortfall true-up. |
| **True-up obligation** | Period-end commercial adjustment surfaced as a structured `TrueUpObligation` on the evaluation result (amount, period, contract ref) for Billing — not a silent in-engine charge. |
| **Mid-cycle change** | A `PriceWindow` or overlay whose catalog `effectiveFrom` falls inside the subscriber's current invoice period (billing anchor may differ from calendar month). |
| **Retroactive pricing** | Any rule assigning a rate to usage based on a policy decision time earlier than operational processing time (late-arrival, administrative repricing). Distinct from normal effective-dated windows. |
| **PriceWindow** | Non-overlapping, UTC-bounded interval during which a `Price` row is effective. Step-2 selection is on the pricing **canonical 8-axis scope key** `(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)` (pricing `ADR/0001` + `ADR/0002`, adopted **verbatim**). `chargeKind` (hybrid recurring vs usage), `priceEligibility`, and `cohort` (grandfathering generation) are additive axes, so multiple rows legitimately coexist at one `(planId, currency, region, phase)`; the non-overlap invariant and 'at most one match' hold only on the **full** key. |
| **PriceOverlay** | A scoped collection of price overrides with `scope(customerGroup \| partner \| orgTier \| brand \| region \| global)` and explicit `precedence`. Eligibility resolved against evaluation-context fields before precedence stacking. |
| **Coupon** | Promotional discount instrument (id, type, validity, applicability, redemption limits, campaigns). Entity lifecycle/campaign management owned by Promotions; this PRD owns when and how an eligible coupon adjusts a resolved charge line. |
| **Coupon stacking policy** | `exclusive_best` (default — single winning coupon) or `ordered_stack` (explicit campaign-linked sequence only). |
| **RatingRule** | Defined in manifest §4.2 — maps resolved price outcome to Usage → RatedCharge in Rating. Not redefined here. |
| **SSP (Standalone Selling Price)** | Price at which an entity would sell a promised good/service separately; an input to ASC 606 allocation. Carried as references on charge lines; recognition schedules are out of scope. |

## 2. Architecture Alignment

| **Field** | **Value** |
|-----------|----------|
| **Applicable Manifest(s)** | BSS |
| **Relevant Chapters** | §4.1 Product and Service Catalog; §4.2 Rating and Charging; §4.4 Billing and Invoicing (snapshot/immutability contract); §2.1.3 Multi-tenant semantics; §8 Data and Domain Model (identity invariants) |

> **Normative alignment**: extends manifest requirements for **commercial price resolution** and **deterministic rating inputs**. MUST NOT contradict: (a) Catalog as SoR for Product/SKU/Plan/Price/PriceWindow/PriceOverlay/CatalogVersion; (b) Rating as deterministic Usage→RatedCharge→BillableItem pipeline; (c) posted financial immutability with corrections via adjustments/credit/debit notes; (d) OSS/BSS boundary (BSS MUST NOT mutate OSS topology or Policy Engine state).

> **Manifest extension (PriceWindow coverage)**: manifest §4.1 guarantees non-overlapping windows for a key; this PRD requires that key to be the pricing **8-axis canonical scope key** — `(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)` (pricing `ADR/0001` + `ADR/0002`) — and additionally requires **no gaps** for billable usage at `t` (if no window matches, evaluation MUST fail explicitly — AC 6).

> **Deployment (normative for Design)**: the evaluation core (**`rating-core`**) is a **pure, I/O-free crate inside the single `rating` gear/deployable** — consolidation per [ADR-0002](./ADR/0002-cpt-cf-bss-rating-adr-rating-gear-consolidation.md), which supersedes the earlier placement note that treated the evaluation core as a logical module within the BSS Rating domain; the constraint it protected (one deployable, no separate evaluation service) is preserved and strengthened to a compiler-checked crate boundary.

### 2.1 Terminology and Naming

| **Name** | **Usage** |
|----------|-----------|
| **Rating** | Canonical name of this gear and its domain (manifest §4.2): the evaluation core plus the operational rating pipeline — one gear, one deployable (ADR-0002). |
| **rating-core** | The pure evaluation core (crate): deterministic price resolution (§6.3 / §17.1 steps 1–9) over frozen inputs, no I/O. Successor of "tariff-core"/"PLAL" (the pre-ADR-0002 names); use at implementation/abstraction boundaries. |
| **Rating pipeline** | The operational half: usage ingestion & normalization, windowed `Q` (single-writer), usage/delta dedup, evaluation-unit synthesis & the period tick, rated-output persistence, `CommitmentBalanceEffect` publication, Billing handoff (design slices 12–16). |
| **Evaluation** (historically "evaluation") | The deterministic process resolving effective commercial prices and charge formulas for a given context (§6.3). Produces a resolved price outcome + `pricingSnapshotRef`. |
| **Tariff** | Reserved for the **Pricing gear's rate definitions** (rate-card sense: resource @ price, models, windows). Since ADR-0002 it no longer names this gear or its process. |
| **Tariffs / PLAL / tariff-core** | Deprecated names for this gear / its core — do not use in new text (ADR-0002); historical occurrences in the evaluation slices read per the terminology bridge in [DESIGN.md](./DESIGN.md). (2026-07-28 fix: the mechanical rename had rewritten this deprecated-names row into "Rating / rating-core / rating-core", making the canonical names self-deprecating.) |

### 2.2 Predecessor PRDs and Scope Migration

This PRD specializes or supersedes the following scope from predecessor documents:

- **PRD-metering-pricing-module-202601120119** — "Pricing Hierarchy Orchestration (Contract > PriceOverlay > Catalog)" and "Tiered Pricing Calculator" (P0 Rating scope) move here as §6.3 evaluation order (steps 1-9) and formula definitions. The metering/collection half remains authoritative there.
- **PRD-product-catalog-marketplace-202601120119** — "Plan & Price Modeling", "Effective Dating & Price Windows", and "Price Overlays & Adjustments" define the **data primitives** this PRD evaluates; Catalog remains SoR for those primitives. Evaluation semantics and override resolution are authoritative here.
- **PRD-rating-engine-202604031200** (VHP-810) — **absorbed by this PRD (ADR-0002)**: its HIGH scope items ("Tiered pricing evaluation", "Deterministic outputs + pricingSnapshotRef") are specified here (§6), and the pipeline scope it stubbed (Usage → RatedCharge, dedup, windowed `Q`, evaluation-unit synthesis, persistence, Billing handoff) is authored as design slices 12–16 of this gear. The upstream copy is legacy provenance (not maintained).

## 3. Actors

### 3.1 Human Actors

#### Product Manager

**ID**: `cpt-cf-bss-rating-actor-product-manager`

**Role**: Defines plans, meters, tier semantics, and effective windows so commercial behavior is explicit.
**Needs**: Tariff/price-book editor, model configuration (flat/tiered/volume/hybrid/commit), UTC windows, approval submit.

#### Partner Admin

**ID**: `cpt-cf-bss-rating-actor-partner-admin`

**Role**: Applies scoped markups/discounts with precedence so channel economics are controlled.
**Needs**: OrgTier scope selection, adjustment stack definition, non-overlap validation, simulation.

#### Finance Analyst

**ID**: `cpt-cf-bss-rating-actor-finance-analyst`

**Role**: Previews invoice impacts of a future window so forecasts and ASC inputs are explainable.
**Needs**: Sample-usage profiles, candidate-window selection, evaluation-trace export.

#### Platform Operator

**ID**: `cpt-cf-bss-rating-actor-platform-operator`

**Role**: Owns deterministic, hierarchical price resolution so usage-based revenue is reproducible, auditable, and compatible with Rating and Finance controls.
**Needs**: Audit of rule versions, segregation of duties on publish, deterministic replay.

### 3.2 System Actors

#### Rating Pipeline (intra-gear)

**ID**: `cpt-cf-bss-rating-actor-rating`

**Role**: The operational half of this gear (design slices 12–16) — consumes the core's resolved price outcome in-process; produces `RatedCharge` / `BillableItem`; owns the Usage → RatedCharge pipeline, usage/delta dedup, windowed `Q` aggregation (single-writer per `(subscription, meter, dimensionKey, window)`), evaluation-unit synthesis incl. the period tick, rated-output persistence, and `CommitmentBalanceEffect` publication. Listed with the system actors because the evaluation-core FRs (§6) reference it as their operational counterpart; since ADR-0002 it is intra-gear, not an external system.

#### Billing & Invoicing

**ID**: `cpt-cf-bss-rating-actor-billing`

**Role**: Consumes billable items + snapshots; supplies `periodState` (open / closed_posted); posts immutable invoices; executes period-level floor/cap and invoice rounding.

#### Pricing (Product Catalog)

**ID**: `cpt-cf-bss-rating-actor-catalog`

**Role**: the **Pricing (Product Catalog)** gear — SoR for `skuId`, `planId`, `priceId`, `PriceWindow`, `PriceOverlay`, `CatalogVersion`. Owns the `PriceWindow` store, state machine, and UTC activation job, and **produces** the `PriceWindow*` events (pricing D-03); emits schedule-change events. Rating is a read-only consumer/resolver of these.

#### Contracts & Agreements

**ID**: `cpt-cf-bss-rating-actor-contracts`

**Role**: Supplies account-specific price terms, commitments, true-up clauses, and the bounded-composition cap policy.

#### Subscriptions

**ID**: `cpt-cf-bss-rating-actor-subscriptions`

**Role**: Owns effective-dated Plan/Add-on links, subscription state, plan phases, and the plan-change WHEN/asymmetry policy (`changeEffectiveAt`, `changeMode`).

#### Promotions / Discounts

**ID**: `cpt-cf-bss-rating-actor-promotions`

**Role**: Owns the Coupon entity and campaign stacking; supplies frozen coupon snapshots. Rating consumes; never mutates campaigns.

#### Finance (FX)

**ID**: `cpt-cf-bss-rating-actor-finance-fx`

**Role**: Owns FX rate tables and lock policies; rating-core consumes them as frozen inputs and records `fxTableVersion` / locked-rate id.

#### OSS / AMS (Tenant Identity)

**ID**: `cpt-cf-bss-rating-actor-oss-ams`

**Role**: Supplies `tenantId`, delegation proofs, and OrgTier commercial projection targets.

#### OSS Metering (Usage Dimension Population)

**ID**: `cpt-cf-bss-rating-actor-oss-metering`

**Role**: Emits `dimensionKey` values on each UsageRecord (e.g. S3 storage-class / region / operation; VM instance type) and normalized usage quantity. **Critical-path upstream dependency** for dimensional pricing.

## 4. Operational Concept & Environment

### 4.1 Module-Specific Environment Constraints

- **Multi-tenant isolation**: price overlays and contract overrides are tenant-scoped; cross-tenant administration requires delegation proofs; a contract/account overlay MUST NOT leak across payer/seller tenant scope.
- **Time**: all effective dating and window boundaries are in **UTC**; `calendar_month` aggregation is UTC-delimited; `invoice_period` anchors to the subscription billing anchor (UTC-normalized).
- **Determinism boundary**: rating-core is a pure, I/O-free crate within the `rating` gear; it consumes frozen inputs (catalog snapshot, FX tables, coupon snapshots, windowed `Q`) and MUST NOT re-query mutable catalog state at bill-post time for posted periods.
- **Decimal precision**: rating-core emits amounts at precision sufficient for Billing; invoice rounding (per-line vs per-invoice) is applied by Billing, not rating-core. Design fixes intermediate DECIMAL precision for rating-core-emitted amounts.

**Event alignment (manifest §4.1-4.2)**:

- MUST consume: `PriceWindowScheduled`, `PriceWindowActivated`, `PriceWindowExpired`, `PriceWindowCancelled`, `CatalogVersionPublished` (ordering per stream). `PriceWindowCancelled` retracts a pre-cached not-yet-active window that pricing voided (retirement / cutover unwind, operator DELETE).
- MUST NOT require Rating to re-query mutable catalog state at bill-post time for posted periods; the snapshot contract remains authoritative.

> **Gating dependency (critical path for IaaS billing)**: the **usage dimension-population contract** (OSS metering → rating pipeline ingestion → rating-core) is the bottleneck for billing real cloud resources. The BSS side is owned here (Rating admits dimensions via `dimensionKey` and freezes the declared set; Rating passes them through). The external part is **OSS metering emission** of dimension values: until OSS emits them, `dimensionKey` stays the empty tuple and the only workaround is minting a separate meter per dimension combination — exploding catalog cardinality. See §17.3 and §15.

## 5. Scope

### 5.1 In Scope

| **Feature** | **Priority** | **Notes** |
|-------------|--------------|-----------|
| Deterministic evaluation API (conceptual contract) for Rating | `p1` | Resolved rate/tier outcome + `pricingSnapshotRef` + metadata; replay-safe (§6.1). |
| Pricing models: flat, per_unit (per-seat), tiered (graduated), volume (Variant A), package (block), hybrid, committed usage | `p1` | Formal semantics in §6.2; catalog `modelKind` SoR = pricing §17.2; per-seat = `quantitySource`. |
| Versioning & UTC effective dating; non-overlapping windows per manifest invariants | `p1` | Aligns with PriceWindow + PriceOverlay; activation ordered per `(tenantId, aggregateId)`. |
| Multi-currency: price currency, conversion policy, rate-lock hooks | `p1` | rating-core applies Finance FX (step 8); no tax calculation. |
| Override hierarchy: global → region/brand/orgTier/partner → customerGroup → customer/contract with explicit precedence | `p1` | §6.4 + step 4; class order `customerGroup > partner > orgTier > brand > region > global`. |
| PriceOverlay scope → tenant-axis mapping (seller/payer/brand/region) | `p1` | §6.3 / §17.1; AC 13. |
| Tier aggregation window (`Q` reset policy) for tiered/volume models | `p1` | Required on Price/plan policy; AC 12. |
| Plan phases (trial / intro / evergreen) — price resolution per active phase | `p1` | Subscriptions owns structure; step 1 + AC 14. |
| Price eligibility / grandfathering (new vs existing subscriptions) | `p1` | `priceEligibility` on PriceWindow; AC 14. |
| Billing granularity (minimum billable unit per usage price) | `p1` | Round-up before rate; step 3; AC 15. |
| Dimensional pricing — `(meter, dimensionKey)` lines | `p1` | Critical path for a real IaaS catalog; step 3 + AC 3 + AC 18. Depends on the usage dimension-population contract. |
| CAPACITY / reservation pricing (provisioned Disks/IOPS, RI-style) | `p1` | Two flavors at step 6 via `reservationMatch`: consumption (AC 19) and capacity (`capacityCharge`, AC 20). |
| Usage dimension-population contract (BSS side owned here; OSS emission external) | `p1` | Gating dependency. Rating declares/freezes; Rating passes `dimensionKey` through; OSS emits values (external). |
| Level-based (gauge) aggregation — `aggregationFunction ∈ {peak, time_weighted}` via the granule fold | `p1` | Pricing D-44 / T-D-17: window `Q` = Σ granule folds (additive, so every counter invariant is untouched); `aggregationGranularity ∈ {hour, day}` and `maxHold` frozen in `pricingSnapshotRef`; §6.2 `fr-level-aggregation`, AC 5d. Launch drivers: cloudlet peak-per-hour, storage GB-month. No composite co-occurrence at launch. |
| Composite (derived) meter evaluation | `p1` | Formula-as-data over ≥2 published units; pricing Slice 10 delivers the primitive; §6.7. |
| Bundle `sum_of_parts` component summing + effective rev-share pass-through | `p1` | Eval-time summing; rev-share normalized at pricing publish (D-07); §9.2. |
| Coupon application in evaluation (order, stacking, tier/FX interaction) | `p2` | Promotions owns Coupon entity; semantics in §17.2; step 7; AC 16. |
| Mid-cycle price changes: bucket split, proration alignment to UTC cutoffs | `p2` | No posted invoice mutation. |
| Retroactive pricing modes: administrative re-rate → Adjustment deltas only | `p2` | Preserves invoice immutability; ties to Rating `ChargeAdjustment`. |
| ASC 606 alignment hooks: PO tags, SSP snapshot pointers, allocatable amount fields | `p2` | Recognition schedules remain Billing/Finance; Rating supplies traceable inputs. |
| Operator UX for tariff maintenance, simulation, approval thresholds | `p2` | UI screens (DESIGN, frontend). Approval workflow + audit gates are a `p1` dependency of safe evaluation (manifest §4.1 two-person rule). |

### 5.2 Out of Scope

- **API schemas, storage DDL, error code taxonomies** — Design document(s).
- **OSS metering** emission shapes and `UsageRecord` content beyond fields Rating consumes — OSS domain; Pricing consumes aggregated dimensions per the Rating contract.
- **Tax determination** and **statutory invoicing** — Tax Engine / Billing (rating-core MUST NOT compute tax). Handoff: the emitted amount MUST carry **discount lineage** (pre-/post-overlay, pre-/post-coupon amounts and applied ids) so Billing/Tax can choose gross-vs-net treatment; Rating supplies lineage, not ordering.
- **Full revenue recognition subledger** and **ASC 606 automated journal entries** — Finance/Billing; this PRD requires only compatible tagging and amounts.
- **Policy Engine** enforcement and **resource topology** changes — OSS.
- **Coupon / campaign lifecycle** (creation, distribution, redemption limits, fraud controls) — Promotions; Rating consumes frozen coupon definitions at evaluation time only.
- **Spend control and credit risk** — real-time spend stop / limit enforcement is OSS / Policy Engine; post-aggregation spend caps / bill-shock are Billing; credit risk and prepaid gating are Finance. Rating sets the floor/cap **amount** but performs no enforcement or gating. Launch without a hard spend ceiling requires Finance acceptance (§15).
- **`one_time` / `one_time_setup` charge billing** (T-D-18) — not rated: no evaluation unit is synthesized for these `chargeKind`s; Subscriptions/Billing bill them at their qualifying instant (activation / trial conversion) from the frozen `pricingSnapshotRef` amount, with once-per-subscription-lifetime dedup owned there (the at-sale path of the T-D-14 commitment sale). One-time rows still resolve in step-2 selection for coverage/preview/quote.

## 6. Functional Requirements

> **Content boundary**: FRs define WHAT must be resolved (posting/evaluation semantics), not data models or APIs. Concrete schemas, proto definitions, error taxonomies, and mathematical formulas with symbol definitions are owned by the design set — [`DESIGN.md`](./DESIGN.md) and the slice documents under [`design/`](./design/) (the pre-consolidation `DESIGN-tariffs-pricing-logic-*/` path this line used to cite no longer exists — ADR-0002). The full deterministic step order (steps 1-9) is preserved normatively in §17.1.

### 6.1 Deterministic Evaluation

#### Deterministic evaluation API

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-deterministic-evaluation-api`

Tariff evaluation **MUST** expose a conceptual evaluation contract that, for a given evaluation context at timestamp `t` (UTC), produces a **resolved price outcome** (effective rates, pricing model kind, tier thresholds, overlay winners) plus a `pricingSnapshotRef` and evaluation metadata. It **MUST** be replay-safe.

**Rationale**: Rating and Finance require a stable, reproducible outcome to reproduce charges and audits.

**Actors**: `cpt-cf-bss-rating-actor-rating`

#### Pre-purchase evaluation (order-time price preview)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-fr-pre-purchase-evaluation`

Tariff evaluation **MUST** expose a **pre-purchase** evaluation contract for a prospective purchase that has **no subscription yet** — no `subscriptionId`, no `activatedAt`, no bound `cohort`. Inputs are order-scope: plan/price references, quantity, the tenant axes, the `(currency, region)` market derived from the payer's commercial profile, and an evaluation timestamp `t`. The contract **MUST** return per-line resolved amounts **decomposed by `chargeKind`** — `recurring`, `one_time`, `one_time_setup` — and **MUST** flag `usage` components as carrying no committed amount (they price at rating time from the pinned snapshot). It **MUST** return the **catalog-frozen prefix** of the snapshot (`catalogVersion`, resolved price ids incl. `cohort`, eval-policy version) so the caller can pin it, and **MUST NOT** present that prefix as a complete `pricingSnapshotRef` — composition remains owned by `fr-snapshot-carry`, whose `(currency, region)` segment is written by Subscriptions at activation and whose overlay/coupon/FX/commitment segments are written at eval. The outcome is **non-authoritative**: it **MUST NOT** be a billing input, **MUST NOT** post, and **MUST NOT** create evaluation state. Scopes that are not resolvable before a subscription exists — brand-scoped `PriceOverlay` matching, and any `priceEligibility` generation keyed on `activatedAt`/`cohort` — **MUST** fail explicitly rather than silently resolving to a default. Access is `sellerTenantId`-scoped: a caller **MUST NOT** be returned overlay layers it is not entitled to see. Aggregation of the returned per-line amounts into an order-level total is the **caller's** summation and is not price computation.

**Rationale**: A purchase must be priced, displayed and threshold-checked **before** the subscription that anchors evaluation exists; without this contract the order-capture path has no input, and every caller would re-implement step-2 selection and fork it. The predecessor PRD carried this as a partner-facing effective-price preview scope item with its access model unresolved; authoring it here as a contract keeps the selection logic single-sourced.

**Actors**: `cpt-cf-bss-rating-actor-partner-admin`, `cpt-cf-bss-rating-actor-catalog`

#### Single outcome per frozen context (pure-function core)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-single-outcome-determinism`

The determinism contract is stated over the **evaluation unit** (step 3): for `per_event` models a single normalized `UsageRecord`; for any model with `tierAggregationWindow != per_event`, the **window-aggregated quantity `Q`** for the `(subscription, meter, dimensionKey, window)` key — where the aggregation is the row's frozen `aggregationFunction` (`sum`, or the D-44 granule fold `peak`/`time_weighted` summed over granules — additive in every case, `fr-level-aggregation`). Given frozen inputs `(window-aggregated inputs, pricingSnapshotRef, fxTableVersion)`, the monetary outcome **MUST** be identical across replay, recompute, and cross-region batch workers. The windowed `Q` **MUST** be materialized and owned by the **rating pipeline's `QMaterializer`** over `windowed_counter` (Design slice 13; single writer per partition key); **rating-core** receives `Q` as a frozen input and **MUST NOT** aggregate (the §2.1 core/pipeline vocabulary — 2026-07-28 review fix: the post-rename sentence had both halves named "Rating"). Concurrent re-resolve **MUST** serialize on the partition key.

**Rationale**: A pure-function core over frozen, window-aggregated inputs is what makes replay and late-arrival handling non-divergent without cross-partition locks.

**Actors**: `cpt-cf-bss-rating-actor-rating`

#### Snapshot carry

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-snapshot-carry`

Every evaluation **MUST** emit identifiers sufficient for manifest `BillableItem.pricingSnapshotRef` and stable `{skuId, planId, priceId}`. `pricingSnapshotRef` **MUST** be a composite reference over the canonical field list (§1.4): the catalog-frozen subset `{catalogVersion (pending→committed), resolved price ids incl. cohort, eval-policy version}` is **pre-stamped by pricing at publish**; Rating (the **composition SoR**) adds the resolved overlay/`priceOverlay` ids, applied coupon id(s) + stacking policy, FX-lock id, and the commitment/reservation set (reservation match; pool set incl. `poolType`, balances @ `balanceVersion`, draw order, rollover; reserved-vs-pool split) at eval; Subscriptions freezes the `(currency, region)` binding at activation — **not** equivalent to `CatalogVersion` alone.

**Rationale**: Reproducibility requires freezing all commercial inputs, not just the catalog version.

**Actors**: `cpt-cf-bss-rating-actor-billing`

#### Usage and delta idempotency

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-idempotency`

Same usage idempotency key + same snapshot **MUST NOT** double-charge (Rating dedup remains authoritative). Deltas from retroactivity / period-FX close are **new commercial events**, not the original usage key; each delta **MUST** carry a stable correction key `(unitKey[, slice], prior-rated-version, snapshot)` — `unitKey` = the usage counter key `(subscription, meter, dimensionKey, window)` or the period-driven unit key (both unit families, so a close-time FX re-rate of a recurring line or a true-up recompute dedups exactly like a usage correction — Design 01 §4.2); the sub-window slice coordinate is present when a §6.11 split partitions a usage window — so a re-rate retry is idempotent and cannot double-adjust. The owner of delta dedup (Rating or Billing) **MUST** be named in Design before the Adjustment path goes live — **named: Rating** (Design 01 §2.2 / 08 §2.2).

**Rationale**: Deterministic replay and correction safety require distinct, stable idempotency for usage vs deltas.

**Actors**: `cpt-cf-bss-rating-actor-rating`

#### Non-negative resolved price

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-non-negative-price`

A resolved per-line price **MUST NOT** go negative; evaluation **MUST** clamp to zero or emit the residual as a structured credit (clamp-vs-credit policy TBD — §15). Applies **after step 8 — FX and the billing-currency coupon pass** (a billing-currency `fixed_amount` coupon is the first input that can drive a line negative, so the guard clamps the post-FX amount, never the pre-FX one — Design slices 01 §4.4 / 06 / 07; propagated from the design 2026-07-28) and **before** period-level floor/cap.

**Rationale**: Negative resolved lines corrupt downstream rating and revenue; a floor must not mask a negative line.

**Actors**: `cpt-cf-bss-rating-actor-finance-fx`

#### Separation from posted financials

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-separation`

Tariff evaluation **MUST NOT** mutate Usage or posted invoices; retroactive outcomes **MUST** flow through Adjustment paths (manifest §4.2). A correcting/negative usage event **MUST** deterministically reverse its prior commercial effect (refill drawn-down commitment pool, decrement tier counter `Q` for the affected `(subscription, meter, dimensionKey, window)`) and emit compensating deltas; it **MUST NOT** drive a resolved line negative. Correction ingestion and dedup remain Rating.

**Rationale**: Posted-financial immutability and auditable corrections are manifest invariants.

**Actors**: `cpt-cf-bss-rating-actor-billing`

### 6.2 Pricing Models

> **Model-kind SoR (pricing §17.2 kind→formula mapping, adopted verbatim):** the catalog `modelKind` enum is `{flat, per_unit, graduated, volume, package}`. `hybrid` and `committed usage` are **plan-composition / commercial constructs** (a plan carrying multiple `chargeKind` lines; a commitment pool over a base model), **not** `modelKind` values. `graduated` / `volume` / `package` are **usage-only** (`chargeKind=usage`, pricing D-18); tier bands are **always open-top** — no closed top and no above-max fail-closed branch (pricing D-17); capping is a period-level obligation (§6.11).

#### Flat pricing

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-flat-pricing`

Flat charge **MUST** be `unitPrice x Q` (or a fixed amount per period for recurring); no thresholds evaluated.

**Rationale**: The base model must be unambiguous and threshold-free.

**Actors**: `cpt-cf-bss-rating-actor-rating`

#### Per-unit (per-seat) pricing

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-per-unit-pricing`

A per-unit recurring charge **MUST** be `unitPrice × quantity`, where `quantity` comes from the plan's `quantitySource` (`subscription_seat_count`, supplied by Subscriptions, or `manual`) — **never** metered usage `Q`. This is the catalog `modelKind=per_unit` (pricing **p1 launch**); the quantity source is frozen in `pricingSnapshotRef`. Distinct from flat (fixed per-period) and from usage-metered models.

**Rationale**: Per-seat plans are a launch model; the seat count is an external Subscriptions input, not usage.

**Actors**: `cpt-cf-bss-rating-actor-subscriptions`

#### Tiered (graduated) pricing

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-tiered-graduated`

With two or more tiers, each unit **MUST** be charged at its marginal band rate; a single tier rate **MUST NOT** be applied to all units. With one tier only, graduated and Volume Variant A are numerically identical — the distinction is by configured model kind, not by this rule. Tier counter `Q` **MUST** use the configured `tierAggregationWindow`.

**Rationale**: Graduated vs volume must be fixed in writing to prevent rating divergence.

**Actors**: `cpt-cf-bss-rating-actor-rating`

#### Volume Variant A (rate on total Q)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-volume-variant-a`

A single tier rate **MUST** apply to **all** units based on total quantity `Q` within the `tierAggregationWindow`; this variant **MUST** be configured explicitly per SKU and distinguishable from graduated.

**Rationale**: Whole-quantity pricing is a distinct commercial model and must be explicit.

**Actors**: `cpt-cf-bss-rating-actor-rating`

#### Package (block) pricing

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-fr-package-pricing`

A package (block) usage charge **MUST** be `ceil(usedQ / packageSize) × packagePrice` over the configured `tierAggregationWindow` — whole blocks are billed and a partial block rounds **up** to one block. This is the catalog `modelKind=package` (usage-only, pricing D-18); `packageSize` and `packagePrice` are frozen in `pricingSnapshotRef`. Distinct from Volume Variant A (a single per-unit rate on total `Q`). **Volume Variant B (per-tier flat block fee) is not authorable** — the catalog maps `volume` to Variant A only (pricing D Q3).

**Rationale**: Block pricing (e.g. per 1000 API calls) is a distinct construct with round-up-to-block math; per-tier block fees have no catalog authoring home.

**Actors**: `cpt-cf-bss-rating-actor-rating`

#### Level-based (gauge) aggregation

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-level-aggregation`

For a usage row whose frozen **`aggregationFunction ≠ sum`** (pricing D-44: `peak` | `time_weighted`, with `aggregationGranularity ∈ {hour, day}` — both frozen in `pricingSnapshotRef`), the meter is **level-shaped**: normalized usage records are point-stamped **gauge samples** in the level unit. Rating **MUST** derive the window quantity by the **granule fold**: the window is cut into granules per the frozen granularity; each granule folds deterministically (`peak` → the maximum sample in the granule; `time_weighted` → the step-function integral of the level over the granule, `hold_last` bounded by the declared `maxHold` — **an integer count of granules >= 1** (pricing's declaration; the last level legitimately carries **across granule boundaries** for up to `maxHold` granules), beyond which the level reads **0** and an operator signal **MUST** raise — never a guessed value); the window `Q` **MUST** equal the **sum of granule folds** and is therefore additive. All downstream machinery — the `(subscription, meter, dimensionKey, window)` counter key, supersession continuity, per-slice `bandOffsetQ` (T-D-12), band/package math, delta-only corrections — applies to this `Q` **unchanged**. A late or corrected sample **MUST** re-fold its granule and, for `time_weighted`, the up-to-`maxHold` following granules whose held level it changes, producing `Q` deltas under the standard re-materialization discipline. Non-`sum` aggregation **MUST NOT** co-occur with a composite meter at launch.

**Rationale**: The launch product set bills on levels — cloudlet peak-per-hour and storage GB-month — and the commercial rule (which fold, which cadence) must live in the catalog, not be pre-folded inside the emitting source (which would hide raw levels from audit and make retro re-aggregation impossible). Summing granule folds keeps `Q` additive so no counter invariant is disturbed (T-D-17).

**Actors**: `cpt-cf-bss-rating-actor-rating`, `cpt-cf-bss-rating-actor-oss-metering`

#### Hybrid pricing

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-hybrid-pricing`

Recurring and usage components **MUST** be emitted as **two distinct lines** under one `planId` (so Billing can itemize), each evaluated independently per its period boundaries. A hybrid "minimum commitment" **MUST** be expressed as committed-usage (commitment pool + overage, step 6); a minimum monthly invoice fee (period floor) is a separate period-level construct (§6.11, §17.2) and **MUST NOT** be conflated. Attachment points: commitment (step 6) and floor/cap attach to the usage line unless the plan marks them plan-level; coupon (step 7) attaches per `applyScope` (`usage` / `recurring` → that line; `line_total` → combined total, applied once as a plan-scoped overlay and split back pro-rata across the two lines deterministically). The attachment configuration **MUST** be frozen in `pricingSnapshotRef`.

**Rationale**: Itemization, min-commit disambiguation, and deterministic coupon splitting are required for auditable hybrid plans.

**Actors**: `cpt-cf-bss-rating-actor-billing`

#### Committed usage

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-committed-usage`

In-commitment and overage portions **MUST** be charged at distinct rates over the ordered `commitmentPools[]` (step 6). In-commit **billability** follows the pool's frozen `poolType`: `prepaid_drawdown` (pool billed upfront at sale, outside rating-core; in-commit consumption emits a due-zero line with notional lineage) or `committed_rate` (in-commit bills in arrears at the committed rate). Period true-up follows the contract and **MUST** be surfaced as a structured `TrueUpObligation` (amount, period, contract ref) for Billing — not an implicit posted charge; for `committed_rate` pools the shortfall formula is normative (quantity basis: unmet committed quantity × committed rate; spend basis: unmet committed spend), and `prepaid_drawdown` pools surface no pool-driven true-up (unused balance follows the rollover policy). A correcting/negative usage event **MUST** refill the drawn-down pool and emit compensating deltas; it **MUST NOT** drive the resolved line negative.

**Rationale**: Prepaid/overage economics and true-ups must be explicit and reversible.

**Actors**: `cpt-cf-bss-rating-actor-contracts`

### 6.3 Rule Evaluation Order

> The full normative step order (steps 1-9, plus the reserved-capacity and period-level phases) is preserved verbatim in §17.1. The FRs below carry the requirements that the order enforces.

#### Deterministic evaluation order

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-evaluation-order`

For any evaluation at `t` (UTC) and context `ctx`, the engine **MUST** apply the fixed order: (1) subscription composition + active phase, (2) base catalog row selection, (3) meter mapping + billing granularity, (4) partner/OrgTier/brand/region overlays, (5) customer/contract overlay, (6) commitment + reservation, (7) coupon, (8) FX, (9) emit. The step order is **invariant** for every contract — there is **no reordering knob**. Replay over identical inputs **MUST** be byte-identical.

**Rationale**: A single invariant order is the basis of determinism (AC 1).

**Actors**: `cpt-cf-bss-rating-actor-rating`

#### Base catalog row selection

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-base-catalog-selection`

Step 2 **MUST** select `Price`/`PriceWindow` such that `t in [effectiveFrom, effectiveTo)` on the pricing **8-axis canonical scope key** `(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)` per the non-overlap invariant. Eligibility class order is `existing_grandfathered > new_subscriptions_only > all_subscriptions` (most-specific-wins). **Within `existing_grandfathered`, the generation is selected by the subscription's bound `cohort`** — the `cohort` of its **pinned price id** in `pricingSnapshotRef` (pricing `ADR/0002`), never `activatedAt` alone. At most **one** window MUST match on the full key (coexisting hybrid `chargeKind` rows and `cohort` generations are disambiguated by the key, not fail-closed). If no eligible window matches, evaluation **MUST** fail (no silent fallback) for billable usage. When invoice currency equals the row's price currency, step 8 FX is skipped.

**Rationale**: Gap/overlap-free, eligibility-correct selection prevents silent mispricing.

**Actors**: `cpt-cf-bss-rating-actor-catalog`

#### Meter mapping and billing granularity

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-meter-mapping-granularity`

Step 3 **MUST** map `UsageRecord` to a charge line keyed by `(meter, dimensionKey)`; the mapping **MUST** be injective on `(meter, dimensionKey)` per plan revision, or reject as a configuration error (fail-closed). `billingGranularity` round-up **MUST** be applied to the **aggregated/merged measure** of the evaluation unit, **never per raw `UsageRecord`** (twelve 5-minute samples at `per_hour` MUST bill 1 hour, not 12). The merge/aggregation is owned by the **rating pipeline** (slice 13; single-writer per `(subscription, meter, dimensionKey, window)`); **rating-core** prices the normalized aggregate.

**Rationale**: Injective mapping and aggregate-level round-up prevent line collisions and ephemeral over-charge.

**Actors**: `cpt-cf-bss-rating-actor-rating`

#### PriceOverlay scope → tenant-axis mapping

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-priceoverlay-scope-mapping`

Step 4 **MUST** resolve `PriceOverlay.scope` against evaluation context: `global` always eligible; `customerGroup` matches the payer's BSS-resolved customer group at `t` (from authenticated caller claims; most-specific class); `partner` / `orgTier` match `sellerTenantId`; `brand` matches Plan/SKU `brandId` at `t`; `region` matches the usage or price-row `region` key. `resourceTenantId` **MUST NOT** alone match partner/orgTier lists; `payerTenantId` / `accountId` are used for contract/account overlays in step 5, not via `PriceOverlay.scope`.

**Rationale**: Correct scope→axis mapping prevents cross-tenant price leakage (AC 13).

**Actors**: `cpt-cf-bss-rating-actor-oss-ams`

### 6.4 Override Hierarchy and Overlays

#### Overlay stacking (partner / OrgTier / brand / region)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-overlay-stacking`

Step 4 **MUST** apply all scope-matching `PriceOverlay` survivors as a sequential stack in a deterministic total order: ascending `precedence` (lower first); for **cross-class** ties (precedence is unique only *within* a scope class) the pricing **class-specificity order** `customerGroup > partner > orgTier > brand > region > global` (pricing `design/09`, adopted **verbatim**) governs, with ascending `priceOverlayId` only as the final within-class stable tie-break. This layer **stacks** (applies all survivors); the class order breaks ties, it does **not** pick a single winner. Equal `precedence` among lists with overlapping scope **within one class MUST** be rejected at publish validation (fail-closed); the class order + `priceOverlayId` tie-break is the runtime safety net that **MUST** still produce a single deterministic result.

**Rationale**: Deterministic stacking with fail-closed publish validation prevents undefined precedence outcomes (AC 2).

**Actors**: `cpt-cf-bss-rating-actor-catalog`

#### Customer / contract overlay

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-customer-contract-overlay`

Step 5 **MUST** apply contract/account-level overrides after step 4, bounded by entitlement and approval rules; contract terms outrank partner lists (Contract > Partner price overlays > Catalog base). Overrides **MUST NOT** introduce metering dimensions absent from the published Plan/SKU revision; contract publish validation **MUST** reject such overlays (fail-closed). Customer-layer changes **MUST NOT** silently weaken audit controls (Contract workflow + optional Finance approval).

**Rationale**: Contract precedence and dimension integrity must hold without weakening controls.

**Actors**: `cpt-cf-bss-rating-actor-contracts`

#### Bounded composition (anti-drift cap)

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-fr-bounded-composition-cap`

The cumulative markup/discount across the full partner → reseller → customer overlay chain **MUST** be bounded by a configured cap (`maxCumulativeMarkup`). When the stacked result would exceed the cap, evaluation **MUST** clamp to the cap and record the clamp in metadata (or fail-closed if the cap is marked hard); it **MUST NOT** silently compound unbounded markup. For a material multi-link chain, absence of a configured cap **MUST** be fail-closed at publish (or a Finance-set default applied); a publish-time warning is acceptable only for a single-link, non-material overlay. Default cap value and clamp-vs-fail mode are a policy decision (§15).

**Rationale**: Unbounded markup compounding across the channel chain is a commercial-integrity risk.

**Actors**: `cpt-cf-bss-rating-actor-contracts`

### 6.5 Tier Aggregation, Eligibility, Phases, Granularity

#### Tier aggregation window

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-tier-aggregation-window`

Tiered/volume models **MUST** use the configured `tierAggregationWindow` (`calendar_month` \| `invoice_period` \| `subscription_lifetime` \| `per_event` \| `per_hour`) to govern when tier counter `Q` resets. Window boundaries: `calendar_month` in UTC; `per_hour` in UTC clock hours (pricing D-313, and the boundary the `hour` granule already uses); `invoice_period` anchored to the subscription billing anchor per the catalog `billingAnchorPolicy ∈ {calendar_month, subscription_start, fixed_day(d)}` with the no-drift month-end clamp (31→28→31, anchor day preserved; pricing D-20), frozen in `pricingSnapshotRef`. The active value **MUST** be recorded in evaluation metadata and frozen in `pricingSnapshotRef`.

**Rationale**: Tier-counter reset policy is commercially significant and must be explicit and frozen (AC 12).

**Actors**: `cpt-cf-bss-rating-actor-rating`

#### Price eligibility and grandfathering

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-price-eligibility-grandfathering`

Step 2 **MUST** apply `priceEligibility` with the class order `existing_grandfathered > new_subscriptions_only > all_subscriptions` (most-specific-wins, pricing `design/07`): `new_subscriptions_only` excludes subscriptions with `activatedAt` before the window `effectiveFrom`; `existing_grandfathered` includes only subscriptions activated before cutover. **Multi-generation grandfathering (pricing `ADR/0002`):** many generations — distinct `cohort`s, each an active window — may coexist on one key; Rating **MUST** select the row whose `cohort` equals the `cohort` of the subscription's **pinned price id** in `pricingSnapshotRef`, never `activatedAt` alone. If no eligible price applies, evaluation **MUST** fail (no silent fallback).

**Rationale**: New-vs-existing eligibility and grandfathering are first-class commercial rules (AC 14).

**Actors**: `cpt-cf-bss-rating-actor-subscriptions`

#### Plan phases

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-plan-phases`

Step 1 **MUST** resolve the active plan **phase** at `t` (trial / intro / evergreen or successor phases per Subscriptions SoR); the phase selects the applicable price schedule within the plan. Distinct phases MAY have schedules that coexist at the same `t` — this is not an overlap, since `phase` is part of the PriceWindow key (a uuid `phase_id`, pricing D-19). **Usage rows are phase-invariant by default (pricing D-15):** one usage row spans all phases; an explicit phase-scoped usage row wins for its phase (most-specific-wins). The no-gap rule applies to the *resolved* set — a phase covered only by a phase-invariant usage row is **not** a gap.

**Rationale**: Phase-correct selection is required for intro/evergreen plans (AC 14).

**Actors**: `cpt-cf-bss-rating-actor-subscriptions`

#### Billing granularity round-up

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-billing-granularity`

Usage duration/quantity **MUST** be rounded **up** to the configured `billingGranularity` (`per_second` \| `per_minute` \| `per_hour` \| `per_day` \| whole-unit) before rate application, on the **merged/aggregated** measure (not per raw record). `billingGranularity` **MUST** be recorded in evaluation metadata. A per-resource `minimumCharge` MAY be configured to bound ephemeral-resource over-charge (§15).

**Rationale**: Minimum billable unit must be deterministic and applied at the aggregate (AC 15).

**Actors**: `cpt-cf-bss-rating-actor-rating`

### 6.6 Commitments and Reservations

#### Commitment drawdown, overage, true-up

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-commitment-drawdown`

Step 6 **MUST** apply drawdown/overage per contract over an ordered list of commitment pools (`commitmentPools[]`, Contracts SoR), in declared order (waterfall): each pool absorbs quantity/spend up to its remaining balance before the next; residual beyond all pools is overage / on-demand. A single pool is the default special case. **How the residual is priced is selected by the frozen `overageRate` field (T-D-19):** present on the `commitmentReservation` segment ⇒ the residual bills at that **flat frozen rate** (no band math on the residual); absent ⇒ the residual bills at the **banded on-demand rates**, placed over the **post-reservation remainder `Q`** — the reservation exclusion of §6.6 applies first (matched quantity removed, remainder banded from zero), and pool draws then **never** reduce the counter (the pool-vs-reservation asymmetry). With no reservation on the line the remainder is the full `Q`. The selector is the field's **presence**, frozen at contract publish; Rating consumes the rate values and never decides them. The frozen pool set, per-pool balances, draw order, rollover policy, and any reserved-vs-pool split **MUST** be carried in `pricingSnapshotRef`. Commitment is **always** evaluated at step 6 (no reordering).

**Rationale**: Deterministic waterfall drawdown with frozen pool state is required for reproducible committed-usage billing.

**Actors**: `cpt-cf-bss-rating-actor-contracts`

#### Reservation pricing — consumption-flavor

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-reservation-consumption-flavor`

When a consumption-flavor `reservationMatch` is present, the **matched portion** of measured usage **MUST** be priced at the **reserved rate** — self-service reserved rates are sourced from `pricingSnapshotRef` / the catalog snapshot; negotiated RI-style rates from the Contracts overlay at step 5 (pricing `PRD:935`) — and the remainder at on-demand rates resolved in steps 2-5. The reserved portion **MUST** be excluded from `commitmentPools[]` drawdown (reservation precedes pools) **and from the on-demand tier counter `Q`** — the remainder re-bands from zero (pricing `inst-rv-tier-q`); the in-commit pool quantity is **NOT** excluded (pool-vs-reservation asymmetry). The reservation-match identifier **MUST** be recorded in metadata and `pricingSnapshotRef`. With no `reservationMatch`, evaluation prices as pure usage.

**Rationale**: Reserved-rate coverage of measured usage (RI-style) must be deterministic and pool-precedent (AC 19).

**Actors**: `cpt-cf-bss-rating-actor-contracts`

#### Provisioned-capacity charging — capacity-flavor

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-capacity-charge`

When a capacity-flavor `reservationMatch` with `reservedQuantity` is present, evaluation **MUST** emit a `capacityCharge` = reserved rate x `reservedQuantity`, **regardless of measured usage** (zero usage still bills the allocation). The `capacityCharge` **MUST NOT** be reduced by absent usage and **MUST NOT** draw down `commitmentPools[]`. `reservedQuantity`, reserved rate, and flavor **MUST** be frozen in `pricingSnapshotRef`.

**Rationale**: Provisioned disks/IOPS bill on allocation, not consumption (AC 20).

**Actors**: `cpt-cf-bss-rating-actor-contracts`

### 6.7 Dimensional (Cloud) Pricing

#### Dimensional pricing — `(meter, dimensionKey)` lines

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-dimensional-pricing`

Each distinct `(meter, dimensionKey)` (e.g. S3 storage-class / region / operation; VM instance type) **MUST** resolve to its own charge line and price, with no line collision (injective per the step-3 rule). The declared dimension set **MUST** be frozen in `pricingSnapshotRef`. A plan that declares no dimensions prices as a single empty-tuple line. A record arriving with empty or partial dimension values on a dimension-declaring plan **MUST NOT** be silently priced as a single line; evaluation **MUST** route it to an explicitly published default/catch-all line (if defined) or fail-closed (reject/quarantine) — never guess.

**Rationale**: Real IaaS catalogs require per-dimension pricing without collapsing or guessing dimensions (AC 18).

**Actors**: `cpt-cf-bss-rating-actor-rating`

#### Usage dimension-population contract (BSS side)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-dimension-population-contract`

The catalog (pricing) **persists** the declared dimension set on the Plan/SKU revision (`dimension_key`, structurally in-scope now); Rating **freezes** that declared set in `pricingSnapshotRef`; Rating passes `dimensionKey` through. Declaration authoring is catalog-owned — Rating owns only the freeze. Dimension **value pricing** stays OSS-emission-gated. Value **emission** on usage is OSS metering (external upstream requirement). Until OSS emits dimension values, `dimensionKey` stays the empty tuple and per-combination meters are the only workaround (exploding cardinality — tracked as critical-path risk, §16).

**Rationale**: The BSS side of the dimension contract is closeable now; the OSS emission is the gating critical path.

**Actors**: `cpt-cf-bss-rating-actor-oss-metering`

#### Composite (derived) meter evaluation

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-composite-meter-eval`

When a plan carries a **derived (composite) meter** — a formula-as-data over ≥ 2 published input units producing one output unit, declared and delivered by the pricing gear (Slice 10) — Rating **MUST** evaluate the frozen formula to the output quantity, then price the output unit by its `modelKind` per steps 2-9. The formula, its input unit set, and the output unit **MUST** be frozen in `pricingSnapshotRef`; Rating **MUST NOT** author or mutate the derivation (catalog-owned). Composite **input** derivation is window-`sum`; non-`sum` aggregation (`peak`/`time_weighted`, pricing D-44 / `fr-level-aggregation`) **does not co-occur** with composite meters at launch (`last`/`unique` remain Future).

**Rationale**: A real VM line (vCPU + RAM as one priced unit) requires evaluating a catalog-declared composite formula deterministically; the upstream primitive now lands in pricing at launch.

**Actors**: `cpt-cf-bss-rating-actor-rating`

### 6.8 Coupons (Promotions Overlay)

> Coupon entity lifecycle and campaign management are owned by Promotions (cross-PRD). Rating owns deterministic application semantics. Full placement/stacking/consumption contract preserved in §17.2.

#### Coupon application order

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-fr-coupon-application-order`

Coupons are an overlay on resolved commercial price, applied at **step 7** after steps 4-6 (post-commitment line amount). Default: `settlementCurrency = price` coupons apply in price currency before FX (step 8); `settlementCurrency = billing` coupons apply after step 8 on the billing-currency amount (same `fxTableVersion`). The applied coupon id(s) and pre-/post-discount amounts **MUST** be recorded in metadata.

**Rationale**: Deterministic coupon placement relative to overlays, commitment, and FX is required to reproduce charges (AC 16).

**Actors**: `cpt-cf-bss-rating-actor-promotions`

#### Coupon stacking and conflicts

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-fr-coupon-stacking`

Default stacking is `exclusive_best` (select the single coupon yielding the largest customer benefit; others MUST NOT apply on the same line). **Cross-scope exclusivity (T-D-20):** when the candidate set contains a `line_total`-scoped coupon, the exclusivity set **widens to the plan** — exactly one coupon applies **per plan per period**, the winner being the candidate with the greatest total benefit measured on its own scope; without this, a `usage`-scoped and a `line_total`-scoped coupon are "not on the same line" and both apply, i.e. double discounting under the default policy. `ordered_stack` applies only when a Promotions campaign explicitly links coupons with `stackSequence` (ascending; each step uses the prior output). Campaign-marked incompatible pairs **MUST** fail-closed at redemption bind time if both would apply. A coupon snapshot omitting `applyScope` (or `stackSequence` under `ordered_stack`) **MUST** fail-closed — Rating **MUST NOT** infer it.

**Launch limitation (T-D-22):** `applyScope = line_total` is **not evaluable at launch** and **MUST** fail closed. Step 7 runs per line and every evaluation unit is sub-plan (§17.1 step 3; a hybrid plan's usage and recurring lines are distinct units on distinct triggers), so no point in the pipeline holds a plan's line set at once — which both `line_total` attachment and T-D-20's cross-scope comparison require. A `line_total` coupon **MUST NOT** be applied to a single line, split against a partially-rated plan, or downgraded to another scope. Defining the plan-period assembly point, its trigger, and its cascade behaviour is a named joint gate with Promotions (§15); T-D-20 is inert until then.

**Rationale**: Winner-takes vs ordered stacking must be unambiguous and fail-closed on missing policy (AC 16).

**Actors**: `cpt-cf-bss-rating-actor-promotions`

### 6.9 Multi-Currency and FX

#### Multi-currency separation

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-multi-currency`

The engine **MUST** separate **price currency** (the selected `Price.amount` row; distinct per-market rows are first-class, not FX-derived), **billing currency** (invoice currency per payer account/contract), and **presentment currency** (portal display FX, non-authoritative and outside rating-core — such amounts MUST be labelled estimates). Conversion applies only when billing currency != row currency.

**Rationale**: Conflating list price, settlement currency, and display FX causes disputes.

**Actors**: `cpt-cf-bss-rating-actor-finance-fx`

#### FX policy (rating-core abstraction)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-fx-policy`

When invoice currency != price currency, rating-core **MUST** apply the FX table per Finance policy and record `fxTableVersion` or locked-rate id; it **MUST NOT** use implicit/provider-default FX without a policy record. Two deterministic policies: (a) **per-window rate-lock** — final at event time; (b) **invoice-period FX** — emit a **provisional** amount at the locked/spot rate (flagged provisional) on the hot path and **re-rate by delta at period close** via the Adjustment path (close-time `fxTableVersion` is authoritative). Replay over identical inputs (including which `fxTableVersion` applied at which stage) **MUST** be byte-identical.

**Rationale**: Explicit, recorded FX with provisional+delta close keeps the hot path fast and replay byte-identical (AC 8).

**Actors**: `cpt-cf-bss-rating-actor-finance-fx`

### 6.10 Retroactivity and Corrections

#### Posted-period protection

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-posted-period-protection`

When `periodState = closed_posted`, a retroactive price change to usage in that period **MUST NOT** alter posted invoice lines and **MUST** generate **delta** adjustments consumable by Billing per immutability rules. Retroactive runs **MUST** separately record usage-observation time and pricing-policy decision time in the audit log.

**Rationale**: Posted financials are immutable; corrections flow as auditable deltas (AC 9).

**Actors**: `cpt-cf-bss-rating-actor-billing`

#### Late-arriving usage into an aggregate window

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-fr-late-arriving-usage-reresolve`

For a graduated/volume model over `tierAggregationWindow != per_event` with `periodState = open`, late usage arriving after some events were rated **MUST** trigger deterministic re-resolution of tier placement for the whole window-aggregated `Q` and emit **DELTA** adjustments for already-rated events (no mutation of prior outputs), re-resolved **strictly from the pinned `pricingSnapshotRef`** (no live catalog read; when a §6.11 split partitions the window, per sub-window slice — each slice replays its **own** pin, coupled to earlier slices only via the frozen band offset). The **one sanctioned exception (T-D-21)** is the **administrative re-rate**: a *policy* correction (corrective publish / historical import — always-material, two-person, pricing-governed) replays over the **superseding** snapshot, because the pin itself is what is being corrected; every *input* correction stays strictly on the pin. With `periodState = closed_posted`, the correction follows posted-period protection. A missing `periodState` **MUST** fail-closed (no guessing).

**Rationale**: Open-window late arrivals must re-resolve deterministically without mutating prior outputs (AC 10).

**Actors**: `cpt-cf-bss-rating-actor-rating`

#### Usage corrections / negative quantity

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-fr-usage-corrections`

A correcting/negative usage event **MUST** deterministically reverse its prior commercial effect: refill the drawn-down commitment pool and decrement tier counter `Q` for the affected `(subscription, meter, dimensionKey, window)`, emitting compensating deltas. It **MUST NOT** drive a resolved line negative. Correction ingestion and dedup remain Rating.

**Rationale**: Reversals must be deterministic and non-negative to keep commitment and tier state correct.

**Actors**: `cpt-cf-bss-rating-actor-rating`

### 6.11 Period-Level and Plan-Change Obligations

> These are period-level phases outside the per-line step order (steps 1-9 have no slot). Full boundary contracts in §17.2.

#### Period-level floor/cap obligation

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-fr-period-floor-cap-obligation`

Minimum fee (floor) and maximum charge (cap) per period are **period-level** phases over the aggregated total, applied **after** per-line emission (step 9). Rating **MUST** set the floor/cap amount, currency, and attachment scope and emit a structured `PeriodFloorCapObligation` (amount, comparison basis, period, contract/plan ref); **Billing executes** it (`max(total, floor)` / `min(total, cap)`) during period aggregation. rating-core **MUST NOT** apply the min/max at line aggregation or round. The non-negative guard applies to each line **before** floor/cap; a floor **MUST NOT** mask a negative line. Whether a contractual floor claws back coupon discount is unresolved (§15; default proposal: floor compares post-coupon total).

**Rationale**: Period-level min/max must be reserved as a Billing-executed obligation, not a per-line op.

**Actors**: `cpt-cf-bss-rating-actor-billing`

#### Mid-cycle proration

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-fr-mid-cycle-proration`

When a `PriceWindow` activates during an invoice period, charges **MUST** be computed separately for each sub-window with distinct snapshots, each emitted at **full precision** (no invoice rounding); Billing aggregates and rounds. Any recurring component prorated across the boundary **MUST** use the configured `prorationBasis` frozen in `pricingSnapshotRef`.

**Rationale**: Mid-cycle window changes must split deterministically and defer rounding to Billing (AC 7).

**Actors**: `cpt-cf-bss-rating-actor-billing`

#### Plan-change proration

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-fr-plan-change-proration`

On a plan change at `changeEffectiveAt`, evaluation **MUST** rate planA over `[periodStart, changeEffectiveAt)` and planB over `[changeEffectiveAt, periodEnd)` (half-open, UTC) against each plan's own revision and snapshot, each at full precision (Billing aggregates). The recurring component **MUST** be prorated on the configured `prorationBasis`. Tier `Q` continuity across the boundary **MUST** follow the target plan's snapshot-frozen `usageCounterOnPlanChange` (pricing D-113 / T-D-29 — absence = reset; `carry` per unit-matched shared line only); commitment-pool carry defaults to reset — pricing publishes no pool flag. Corrections to an already-rated portion **MUST** be emitted as deltas. Evaluation **MUST** consume `(changeEffectiveAt, changeMode)` and **MUST NOT** decide the change mode (Subscriptions owns the policy).

**Rationale**: Plan-change splits must be deterministic and consume — not decide — the change mode (AC 17).

**Actors**: `cpt-cf-bss-rating-actor-subscriptions`

### 6.12 Governance and ASC 606 Traceability

#### ASC 606 traceable identifiers

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-fr-asc606-traceable-identifiers`

The resolved price outcome **MUST** always include `performanceObligationRef` and `sspSnapshotPointer` fields (nullable when not applicable). Non-null values **MUST** be immutable once emitted — subsequent catalog changes **MUST NOT** alter an emitted reference. Billing/Finance MAY ignore null fields.

**Rationale**: Downstream revenue allocation requires stable, immutable PO/SSP references — not a recognition engine here (AC 10).

**Actors**: `cpt-cf-bss-rating-actor-billing`

#### Publish approval and audit governance

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-fr-publish-approval-governance`

The two-person approval workflow for material catalog publish is owned by the **pricing Slice 5** approval engine (single engine — Rating does **not** run a second workflow, manifest §4.1 two-person rule). Rating **MUST** register its four publish-time checks as **fail-closed validators** in that pipeline — ambiguous precedence, ambiguous meter mapping, missing anti-drift cap on material chains, and contract overlays introducing undeclared dimensions — and **MUST** emit auditable events with actor, before/after references, and effective times.

**Rationale**: Safe evaluation depends on segregation of duties and fail-closed publish gates before production.

**Actors**: `cpt-cf-bss-rating-actor-platform-operator`

## 7. Non-Functional Requirements

### 7.1 NFR Inclusions

> Targets below are **working assumptions** (baselines from `PRD-metering-pricing-module-202601120119`) pending the program NFR workshop; rows marked TBD MUST be committed before Design lock (§15).

#### Throughput and latency

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-nfr-throughput-latency`

Tariff evaluation **MUST** meet p95 latency targets: **< 100 ms** for catalog price lookup and **< 1 s** for the overall rating path; hot-path throughput **MUST** sustain **>= 10M events/day/region**.

**Threshold**: p95 <= 100 ms catalog lookup; p95 < 1 s overall rating path; >= 10M events/day/region (working assumption; final acceptance at NFR workshop, date TBD).

**Rationale**: Rating is on the monetization critical path; delays become revenue leakage or disputes.

#### Horizontal scale (no cross-partition locks)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-nfr-horizontal-scale`

Horizontal scaling **MUST** avoid cross-partition locks on the evaluation hot path; ordering is per-partition `(subscription, meter, dimensionKey, window)`, not global.

**Threshold**: Zero cross-partition locks on the hot path; per-partition ordering only.

**Rationale**: Cross-partition locking caps throughput and breaks the >= 10M events/day/region target.

#### Audit completeness and segregation of duties

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-nfr-audit-segregation`

Material catalog publishes/overrides run through the **pricing Slice 5** multi-approver workflow (single engine); Rating's registered fail-closed validators **MUST** run on every such publish and **MUST** emit auditable events with actor, before/after references, and effective times.

**Threshold**: 100% of material publishes carry pricing Slice 5 multi-approver sign-off, Rating's validators run fail-closed, and a complete before/after audit event.

**Rationale**: CFO-grade controls and partner trust require segregation of duties and complete audit.

#### Resilience (fail-safe, idempotent retries)

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-nfr-resilience`

When evaluation cannot read a consistent snapshot, it **MUST** fail safe (no partial pricing); retries **MUST** be idempotent.

**Threshold**: Zero partial/best-guess priced outputs under read-model lag; idempotent retry on transient failure.

**Rationale**: Financial correctness requires fail-closed behavior, never best-guess pricing (AC 13).

### 7.2 NFR Exclusions

Explicit dispositions for domains not owned by this PRD (no silent omissions):

- **Tax computation NFRs**: Not applicable — owned by Tax Engine / Billing; rating-core MUST NOT compute tax.
- **Revenue recognition schedule performance**: Not applicable — Finance/Billing own recognition; this PRD supplies tagging/amounts only.
- **Spend-enforcement / real-time stop latency**: Not applicable — OSS / Policy Engine (real-time stop), Billing (post-aggregation cap), Finance (credit risk); Rating sets the amount, performs no enforcement.
- **Frontend UX performance / accessibility (WCAG) / i18n**: Not applicable to this backend PRD — owned by the corresponding frontend DESIGN.

## 8. Five Quality Vectors Analysis

| **Quality Vector** | **Show-Stopper Requirements** | **Rationale** |
|--------------------|-------------------------------|---------------|
| **Efficiency** | Evaluation MUST be cache-friendly (read models, immutable snapshots) and avoid repeated full catalog scans per usage event. | Usage pipelines are volume-heavy; CPU/IO waste raises unit cost of goods sold for cloud metering. |
| **Reliability** | Outcomes MUST be replay-deterministic; failures MUST be explicit (fail-closed), never best-guess pricing. | Financial correctness and partner trust require reproducible charges and defensible audits. |
| **Performance** | Hot-path and batch rating MUST scale horizontally per tenant/partition with bounded p95 latency under peak OSS usage (targets in §7.1). | Rating is on the monetization critical path; delays become revenue leakage or disputes. |
| **Security** | Tenant isolation for price overlays and contract overrides; delegation proofs for cross-tenant administration; immutable audit for changes. | Pricing data is commercially sensitive; cross-tenant leakage is a critical incident class. |
| **Versatility** | The model matrix (flat/per_unit/tiered/volume/package/hybrid/commitment) and overlay hierarchy MUST extend without breaking snapshot contracts to Rating. | Channel business models evolve; rigid pricing cores force expensive parallel systems. |

## 9. Public Library Interfaces

> Rating is a backend pricing module (rating-core within the Rating domain), not a client library. Interfaces below are high-level contracts; concrete API schemas, endpoints, and DDL belong in DESIGN.

### 9.1 Public API Surface

#### Evaluation contract

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-interface-tariff-evaluation`

**Type**: conceptual evaluation contract (shape in Design)

**Stability**: stable (contract intent), schema unstable (Design owns)

**Description**: Given an evaluation context at `t`, returns a resolved price outcome (rates, model kind, tier thresholds, overlay winners), `pricingSnapshotRef`, discount lineage, and evaluation metadata (applied coupons, `tierAggregationWindow`, `fxTableVersion`, granularity). Replay-safe and deterministic.

**Breaking Change Policy**: Major version bump for incompatible request/response changes; snapshot semantics are part of the contract.

### 9.2 External Integration Contracts

#### Rating handoff contract

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-contract-rating-handoff`

**Direction**: **bidirectional, intra-gear** (in-process since ADR-0002 / T-D-16 — not an external integration). Inbound to rating-core: the frozen evaluation context assembled by the pipeline — windowed `Q` from the slice-13 store, the synthesized evaluation unit (slice 14), and the pinned `pricingSnapshotRef`. Outbound from rating-core: the resolved price outcome.

**Protocol/Format**: outbound — resolved price outcome + `pricingSnapshotRef` + obligations (`TrueUpObligation`, `PeriodFloorCapObligation`) + discount lineage; the pipeline (slice 15) persists it and maps it to `RatedCharge` / `BillableItem`, and hands off to Billing (slice 16).

**Compatibility**: Snapshot-referenced and replay-safe; the pipeline owns the Usage → RatedCharge path, dedup, and windowed `Q`, while rating-core stays pure and aggregates nothing. *(2026-07-28 review fix: the Direction line named only the inbound leg while the payload described the outbound one.)*

#### Finance FX input contract

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-contract-finance-fx-input`

**Direction**: required from Finance

**Protocol/Format**: FX rate tables and lock policies with `fxTableVersion`; per-window rate-lock and invoice-period FX modes (Design).

**Compatibility**: Immutable frozen inputs; rating-core records `fxTableVersion` / locked-rate id; no implicit provider defaults.

#### Promotions coupon snapshot contract

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-contract-promotions-coupon`

**Direction**: required from Promotions

**Protocol/Format**: frozen coupon snapshot (`couponId`, `adjustmentType`, `value`, `settlementCurrency`, `applyPerTierBand`, `applyScope`, `stackSequence`, validity, applicability, redemption eligibility) (Design).

**Compatibility**: Fail-closed on missing `applyScope` / `stackSequence` under `ordered_stack`; Rating never infers coupon rules from mutable campaign UI state.

#### Billing periodState / obligation contract

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-contract-billing-periodstate`

**Direction**: bidirectional with Billing

**Protocol/Format**: Billing supplies `periodState` (open / closed_posted); Rating emits `PeriodFloorCapObligation` and full-precision sub-window amounts; Billing aggregates, applies floor/cap, and rounds (Design).

**Compatibility**: rating-core MUST NOT round or apply period-level min/max; Billing owns aggregation and rounding policy id.

#### Catalog / Pricing read-model input contract

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-contract-pricing-readmodel`

**Direction**: required from the Pricing (Product Catalog) gear

**Protocol/Format**: the frozen read-model consumer contract (pricing `design/06`): the canonical 8-axis scope key, `modelKind` → formula mapping (pricing §17.2), tier bands, `priceEligibility` + `cohort` (generation), the `prorationBasis` and `billingAnchorPolicy` enums adopted **verbatim**, the prepaid-grant set, and `{skuId, planId, priceId}` rating-compatibility; PriceWindow state via `PriceWindow*` events (incl. `PriceWindowCancelled`); bundle `sum_of_parts` component sets with **normalized effective rev-shares** (`effective_share_bp`) (Design).

**Compatibility**: adopted **verbatim** (CI gate `pricing.contracts.enum_drift` on the enums); Rating re-resolves open-period corrections strictly from the pinned `pricingSnapshotRef` (no live catalog read); Rating **sums** `sum_of_parts` components at eval and passes effective rev-shares through **untouched** (rev-share normalization is pricing publish-time, D-07); Rating registers its four publish-time checks as fail-closed validators in the pricing Slice 5 approval pipeline (single engine, not a second workflow).

#### Subscriptions input contract

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-contract-subscriptions-input`

**Direction**: required from Subscriptions

**Protocol/Format**: active plan phase (`phase_id`) at `t`; `priceEligibility` inputs (`activatedAt`, bound `cohort` via the pinned price id); `quantitySource` seat count for `per_unit`; the plan-change `(changeEffectiveAt, changeMode)` policy (Design); **the period-fact stream** — `BillableItemCreated(kind=recurring)` per `(subscriptionId, billing period, lineKey)`, money-free, carrying the per-component traceability tuple + `pricingSnapshotRef` + suspended interval(s) with the suspension-billing posture + the period-start `payerTenantId` + the `collectionPaused` marker — consumed as the **synthesis trigger for every period-driven unit** (T-D-33, 2026-08-01; this gear never derives a recurring period itself).

**Compatibility**: Rating consumes — never decides — the change mode; the `(currency, region)` binding is frozen by Subscriptions into `pricingSnapshotRef` at activation.

#### Contracts & Agreements input contract

- [ ] `p1` - **ID**: `cpt-cf-bss-rating-contract-contracts-input`

**Direction**: bidirectional with Contracts & Agreements (a `p1` dependency; added by the 2026-07-31 billing-domain review #11 — the surface was designed across the commitment slices with no contract row anywhere)

**Protocol/Format**: inbound — the ordered commitment-pool set (per-pool id, unit, `poolType ∈ {prepaid_drawdown, committed_rate}`, balance-as-of + `balanceVersion`, draw order, rollover, optional `overageRate`) frozen into the `commitmentReservation` snapshot segment at context assembly; the period true-up clause (`commitmentBasis`, committed quantity/spend); negotiated RI-style reserved rates via the step-5 contract overlay. Outbound — one `CommitmentBalanceEffect` per pool-observing rated outcome (draw, refill, or zero-draw + overage marker — T-D-10/T-D-27), idempotent on the outcome's evaluation/correction key. Inbound triggers — T-D-10 re-resolution cascades for later-`balanceVersion` units and T-D-27 over-draw detections (Design: `design/11` §4.9, `design/05` §4.1).

**Compatibility**: Contracts serializes per-pool `balanceVersion` and owns balances; Rating evaluates over frozen balances only (no live balance read) and never mutates one; the effect event schema is a cross-PRD obligation mirrored in the Contracts design.

## 10. Use Cases

#### Tariff and price-book editing

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-usecase-tariff-editor`

**Actor**: `cpt-cf-bss-rating-actor-product-manager`

**Preconditions**:
- A published Catalog version with SKUs/Plans exists.

**Main Flow**:
1. Select SKU/Plan.
2. Configure model (flat / tiered / volume / hybrid / commit) and tier semantics.
3. Set UTC effective windows and submit for approval.

**Postconditions**:
- A versioned tariff is staged with explicit commercial behavior, pending approval.

**Alternative Flows**:
- **Ambiguous precedence or meter mapping**: publish validation rejects fail-closed.

#### Partner price-overlay management

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-usecase-partner-priceoverlay`

**Actor**: `cpt-cf-bss-rating-actor-partner-admin`

**Preconditions**:
- An OrgTier / partner scope exists for the seller tenant.

**Main Flow**:
1. Select OrgTier scope.
2. Define the adjustment stack with explicit precedence.
3. Validate non-overlap and simulate against sample usage.

**Postconditions**:
- A scope-filtered `PriceOverlay` is staged with deterministic precedence.

**Alternative Flows**:
- **Equal precedence with overlapping scope**: rejected at publish (fail-closed).

#### Finance simulation of a future window

- [ ] `p2` - **ID**: `cpt-cf-bss-rating-usecase-finance-simulation`

> **Deferred to Follow-on (T-D-32, 2026-08-01 — confirmed by the product owner 2026-08-01).** No design slice claims
> this use case, and evaluating a *candidate* (unpublished) `PriceWindow` requires a draft
> read the pin discipline forbids (`design/11` §4.2). A simulation surface is a named future
> increment with its own draft-read carve-out; the partner-overlay use case's "simulate
> against sample usage" step rides the same gate.

**Actor**: `cpt-cf-bss-rating-actor-finance-analyst`

**Preconditions**:
- A candidate `PriceWindow` and a sample usage profile are available.

**Main Flow**:
1. Upload/select a sample usage profile.
2. Pick the candidate window.
3. Export the evaluation trace (rates, overlays, snapshot, ASC inputs).

**Postconditions**:
- Forecast and ASC inputs are explainable from a reproducible trace.

## 11. User Interaction and Design

| **Interface Name** | **Role** | **Steps** | **Mockup Screen** |
|--------------------|----------|-----------|-------------------|
| Tariff / price book editor | As a Product Manager, I define plans, meters, tier semantics, and effective windows so commercial behavior is explicit | 1. Select SKU/Plan<br>2. Configure model (flat/tiered/volume/hybrid/commit)<br>3. Set UTC windows and approval submit | — |
| Partner price overlay manager | As a Partner Admin, I apply scoped markups/discounts with precedence so channel economics are controlled | 1. Select OrgTier scope<br>2. Define adjustment stack<br>3. Validate non-overlap and simulate | — |
| Finance simulation | As a Finance Analyst, I preview invoice impacts of a future window so forecasts and ASC inputs are explainable | 1. Upload/select sample usage profile<br>2. Pick candidate window<br>3. Export evaluation trace | — |

## 12. Acceptance Criteria

> **As a** platform operator **I want** deterministic, hierarchical price resolution **so that** usage-based revenue is reproducible, auditable, and compatible with Rating and Finance controls.

### Price resolution and determinism

**1. Single outcome per frozen context**
- **Given** a fixed evaluation context and frozen `pricingSnapshotRef` inputs
- **When** two workers evaluate the same usage record
- **Then** they MUST produce identical resolved unit rates and pre-tax monetary amounts before the Billing tax stage
- **And** all divergences without input change MUST be treated as defects

**2. Hierarchy application order**
- **Given** global, partner `PriceOverlay`, and customer contract overrides that all apply to the same `planId`
- **When** evaluation resolves price at `t`
- **Then** overrides MUST apply in the order defined in §17.1, the partner-layer stack ordered by ascending `PriceOverlay.precedence`
- **And** the evaluation MUST emit an audit trail of which layer produced the winning values
- **Given** two `PriceOverlay` rows with overlapping scope and equal `precedence`
- **When** publish validation runs
- **Then** publish MUST be rejected (fail-closed); if such a pair reaches runtime, evaluation MUST apply the deterministic cross-class class-specificity order `customerGroup > partner > orgTier > brand > region > global`, then `priceOverlayId` as the within-class final tie-break, and MUST NOT produce an undefined result

**3. Meter ambiguity rejection**
- **Given** a plan/SKU publish where a single `(meter, dimensionKey)` maps to more than one charge line
- **When** Catalog publish validation runs
- **Then** publish MUST be rejected (fail-closed) before reaching production
- **Given** a contract overlay that introduces a metering dimension absent from the published Plan/SKU revision
- **When** contract publish validation runs
- **Then** publish MUST be rejected (fail-closed) per step 5
- **Given** an invalid configuration that reached runtime
- **When** evaluation processes usage for that plan revision
- **Then** evaluation MUST fail-closed and MUST NOT silently pick a default tier

### Pricing models

**4. Graduated vs volume semantics**
- **Given** a tiered SKU configured as graduated with two or more tiers
- **When** `Q` spans multiple tiers
- **Then** charge MUST equal the marginal sum per the graduated rule
- **Given** the same numeric tiers configured as volume Variant A
- **When** `Q` is in tier `k`
- **Then** charge MUST apply `P_k` to the entire `Q`
- **Given** a SKU with only one tier
- **When** evaluation runs as graduated or volume Variant A
- **Then** the monetary outcome MAY be identical; the configured model kind MUST still be persisted in metadata
- **Given** `tierAggregationWindow = calendar_month` and usage in March and April
- **When** tier selection runs for April usage
- **Then** March usage MUST NOT count toward the April tier counter `Q`

**5. Committed usage**
- **Given** a subscription with committed quantity `C_commit` and an overage rate for the period
- **When** measured usage `Q` exceeds `C_commit`
- **Then** evaluation MUST split usage into in-commit and overage portions
- **And** when the contract defines period-end true-up, MUST emit a `TrueUpObligation` (amount, period, contract reference) consumable by Billing — not an implicit posted charge

**5a. Per-unit (per-seat) pricing**
- **Given** a `per_unit` plan with `unitPrice` and `quantitySource = subscription_seat_count`
- **When** the subscription reports `quantity = N` seats at `t`
- **Then** the recurring charge MUST equal `unitPrice × N` (never metered usage `Q`); the frozen `quantitySource` MUST be recorded in `pricingSnapshotRef` (joint pricing fixture — FixtureGate)

**5b. Package (block) pricing**
- **Given** a `package` usage SKU with `packageSize` and `packagePrice`
- **When** `usedQ` in the `tierAggregationWindow` requires `ceil(usedQ / packageSize)` blocks
- **Then** the charge MUST equal `ceil(usedQ / packageSize) × packagePrice` (a partial block rounds up to one block); distinct from Volume Variant A (joint pricing fixture — FixtureGate)

**5c. Composite (derived) meter evaluation**
- **Given** a plan carrying a pricing-declared derived meter (formula-as-data over ≥ 2 published input units, `aggregationFunction = sum`)
- **When** evaluation runs at `t`
- **Then** Rating MUST evaluate the frozen formula to the output quantity, then price the output unit by its `modelKind`; the formula + input unit set + output unit MUST be frozen in `pricingSnapshotRef` and MUST NOT be authored by Rating (joint pricing fixture — FixtureGate)

**5d. Level-based (gauge) aggregation — D-44 / T-D-17**
- **Given** a usage row with frozen `aggregationFunction = peak` (or `time_weighted`) and `aggregationGranularity = hour`, and gauge samples for the window's granules (including one granule with a late-arriving higher sample and one granule with a sampling gap exceeding `maxHold`)
- **When** the window `Q` is derived and the late sample then re-folds its granule
- **Then** `Q` MUST equal the **sum of granule folds** (`peak`: max sample per granule; `time_weighted`: step-integral with `hold_last` bounded by `maxHold` — the gapped granule reads **0** for the uncovered span and an operator signal MUST raise, never a guessed level)
- **And** the late sample MUST change only its own granule's fold, producing a `Q` delta under the standard re-materialization (new `qVersion`, delta-only re-resolution); band/package math and `bandOffsetQ` slice math over this `Q` MUST be unchanged from the `sum` case (joint pricing fixture — FixtureGate; publish of a non-`sum` row is blocked without it)

### Time, versioning, currency

**6. Effective windows**
- **Given** only non-overlapping `PriceWindow` rows for the 8-axis canonical scope key `(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)`
- **When** time `t` is queried for base catalog selection per step 2
- **Then** at most one window MUST match **on the full key** — coexisting `chargeKind` rows (hybrid recurring/usage) and grandfathering `cohort` generations are disambiguated by the key, not a fail-close
- **And** distinct phases, `chargeKind`s, and `cohort`s MAY hold schedules that coexist at the same `t` — not an overlap, since each is part of the key
- **And** if none match, evaluation MUST fail explicitly for billable usage (no silent fallback)

**7. Mid-cycle activation**
- **Given** a `PriceWindow` activating at `effectiveFrom` during an invoice period
- **When** usage spans the boundary
- **Then** charges MUST be computed separately for each sub-window with distinct snapshots, each emitted without invoice rounding (full precision)
- **And** the invoice-period total MUST be computed by Billing as the sum of sub-window amounts followed by Billing's rounding policy — rating-core MUST NOT round at aggregation
- **And** any prorated recurring component MUST use the configured `prorationBasis` frozen in `pricingSnapshotRef`

**8. Multi-currency (rating-core FX abstraction)**
- **Given** price currency differs from invoice currency
- **When** rating-core applies conversion per step 8
- **Then** the FX table version or locked rate id MUST be recorded in the evaluation result
- **And** conversion MUST NOT use implicit provider defaults without a policy record

### Retroactivity and corrections

**9. Posted period protection**
- **Given** an invoice already posted for period `P`
- **When** a retroactive price change is applied to usage in `P`
- **Then** the system MUST NOT alter posted invoice lines
- **And** MUST generate delta adjustments consumable by Billing per immutability rules
- **And** retroactive runs MUST separately record usage-observation time and pricing-policy decision time in the audit log

**10. Late-arriving usage into an aggregate window**
- **Given** a model priced over `tierAggregationWindow != per_event` and `periodState = open`
- **When** usage arrives late into that window after some events were rated
- **Then** evaluation MUST deterministically re-resolve tier placement for the whole window-aggregated `Q` and emit DELTA adjustments for already-rated events (no mutation of prior outputs)
- **Given** `periodState = closed_posted`
- **Then** the correction MUST follow posted-period protection (delta adjustments only)
- **And** a missing `periodState` MUST fail-closed (no guessing)

### ASC 606 traceability

**11. ASC 606 traceable identifiers**
- **Given** a evaluation that produces a charge for a subscription
- **When** the result is emitted to Rating / Billing
- **Then** the resolved outcome MUST always include `performanceObligationRef` and `sspSnapshotPointer` (nullable when not applicable)
- **And** non-null values MUST be immutable once emitted — subsequent catalog changes MUST NOT alter an emitted reference

### Tier aggregation, overlays, and eligibility

**12. Tier aggregation window**
- **Given** a tiered or volume SKU with `tierAggregationWindow = invoice_period`
- **When** usage events occur in two sub-periods of the same invoice period with quantities `Q1` and `Q2`
- **Then** tier counter `Q` MUST equal `Q1 + Q2` within that invoice period (not reset per event unless `per_event`)
- **And** the active `tierAggregationWindow` value MUST be recorded in metadata and `pricingSnapshotRef`

**13. PriceOverlay scope and tenant axes**
- **Given** a partner `PriceOverlay` scoped to `sellerTenantId = Partner-A` and a context with `sellerTenantId = Partner-B`
- **When** evaluation runs step 4
- **Then** the Partner-A list MUST NOT apply (filtered before precedence stacking)
- **Given** a contract/account overlay bound to a specific `payerTenantId` / `accountId`
- **When** evaluation runs for a different payer/account
- **Then** that overlay MUST NOT apply and MUST NOT leak across tenants

**14. Plan phase and grandfathering**
- **Given** a subscription in intro phase until `2026-04-30` and evergreen from `2026-05-01`
- **When** usage at `t = 2026-04-15` is evaluated
- **Then** intro-phase prices MUST apply; evergreen prices MUST NOT
- **Given** a `PriceWindow` with `priceEligibility = new_subscriptions_only` effective `2026-04-01`
- **When** subscription `activatedAt = 2026-01-01` is rated at `t = 2026-04-15`
- **Then** that window MUST NOT apply; a prior grandfathered window or explicit eligibility row MUST apply, or evaluation MUST fail if no eligible price
- **Given** two `existing_grandfathered` generations (distinct `cohort`s, each an active window at `t`) and a subscription whose pinned price id in `pricingSnapshotRef` carries `cohort = C1`
- **When** the subscription is rated at `t`
- **Then** evaluation MUST select the row whose `cohort = C1` (never by `activatedAt` alone); most-specific-wins orders eligibility classes only

**15. Billing granularity**
- **Given** a usage price with `billingGranularity = per_hour` and raw duration `65 seconds`
- **When** evaluation computes chargeable quantity
- **Then** billable quantity MUST be 1 hour (round up), not 65 seconds
- **And** `billingGranularity` MUST be recorded in metadata
- **Given** twelve fragmented 5-minute records for one continuous hour of the same `(meter, dimensionKey)`
- **When** evaluation computes chargeable quantity
- **Then** round-up MUST apply to the merged measure (1 hour billable) — NOT per-record round-up (which would yield 12 hours)

### Promotions and coupons

**16. Coupon application order and stacking**
- **Given** a resolved line after steps 4-6 with partner and contract overlays applied
- **When** two eligible coupons match the same line and stacking policy is `exclusive_best`
- **Then** exactly one coupon MUST apply — the one yielding the lowest charge
- **And** the result MUST record `couponId`, stacking policy, and pre-/post-discount amounts
- **Given** campaign-linked `ordered_stack` with sequence `[C1, C2]`
- **Then** C2 MUST apply to the amount produced after C1
- **Given** a graduated tier line total of 100 and a 10% coupon without `applyPerTierBand`
- **Then** the discount MUST be 10 on the line total, not per marginal band
- **Given** price currency EUR and billing currency USD with a price-currency coupon
- **Then** the coupon MUST apply at step 7 before FX; billing-currency coupons MUST apply only after step 8
- **Given** a hybrid plan whose candidate set holds a `usage`-scoped coupon and a `line_total`-scoped coupon under `exclusive_best`
- **Then** exclusivity MUST widen to one coupon per plan per period — the greater total benefit measured on each candidate's own scope — and MUST NOT let both apply as "different lines" (T-D-20)
- **And** at launch a `line_total`-scoped snapshot MUST instead fail closed (T-D-22 — no plan-scoped evaluation base), never applied to a single line nor downgraded to another scope

### Plan change and proration

**17. Plan-change proration within a period**
- **Given** a subscription changes from planA to planB at `changeEffectiveAt` inside one billing period
- **When** evaluation rates the period
- **Then** it MUST rate planA over `[periodStart, changeEffectiveAt)` and planB over `[changeEffectiveAt, periodEnd)` (half-open, UTC) against each plan's own revision and snapshot
- **And** each sub-window MUST be emitted at full precision (Billing aggregates)
- **And** the recurring component MUST be prorated on the configured `prorationBasis` frozen in `pricingSnapshotRef`
- **And** tier `Q` continuity across the boundary MUST follow the target plan's snapshot-frozen `usageCounterOnPlanChange` (absence = reset; `carry` per unit-matched shared line only — T-D-29), commitment pools defaulting to reset (no pool flag is published)
- **And** corrections to an already-rated portion MUST be emitted as deltas via the Adjustment path
- **And** evaluation MUST consume `(changeEffectiveAt, changeMode)` and MUST NOT decide the change mode

### Cloud resource pricing

**18. Dimensional pricing**
- **Given** a meter with declared dimensions (e.g. S3 storage-class / region / operation) and dimension values present on the usage record
- **When** evaluation maps usage at `t` (step 3)
- **Then** each distinct `(meter, dimensionKey)` MUST resolve to its own charge line and price, with no line collision
- **And** the declared dimension set MUST be frozen in `pricingSnapshotRef`
- **Given** a plan that declares dimensions but a record arrives with empty or partial dimension values
- **Then** the record MUST NOT be silently priced as a single line; evaluation MUST route it to an explicitly published default/catch-all line (if defined) or fail-closed — never guess

**19. Reservation pricing — consumption-flavor**
- **Given** a consumption-flavor `reservationMatch` covering part of the measured usage at `t`
- **When** evaluation runs step 6
- **Then** the matched portion MUST be priced at the reserved rate and the remainder at on-demand rates from steps 2-5
- **And** the reserved portion MUST be excluded from `commitmentPools[]` drawdown
- **And** the matched quantity MUST be excluded from the on-demand tier counter `Q` (the remainder re-bands from zero); the in-commit pool quantity is NOT excluded (pool-vs-reservation asymmetry)
- **And** the reservation-match identifier MUST be recorded in metadata and `pricingSnapshotRef`
- **Given** no `reservationMatch` is present
- **Then** evaluation MUST price as pure usage

**20. Provisioned-capacity charging — capacity-flavor**
- **Given** a capacity-flavor `reservationMatch` with `reservedQuantity` (e.g. 100 GB disk) at `t`
- **When** evaluation runs step 6 and measured usage is zero for the period
- **Then** evaluation MUST emit a `capacityCharge` = reserved rate x `reservedQuantity` (allocation billed regardless of usage)
- **And** the `capacityCharge` MUST NOT be reduced by absent usage and MUST NOT draw down `commitmentPools[]`
- **And** `reservedQuantity`, reserved rate, and flavor MUST be frozen in `pricingSnapshotRef`

### Non-Functional Requirements (Show-Stoppers)

**1. Throughput and latency**
- **Given** peak usage ingestion rates per tenant partition
- **When** evaluation runs on the hot path or batch rating
- **Then** p95 latency MUST meet working-assumption targets (< 100 ms catalog lookup, < 1 s overall rating path) and hot-path throughput MUST sustain >= 10M events/day/region
- **And** horizontal scaling MUST avoid cross-partition locks on the hot path

**2. Audit and segregation**
- **Given** a material tariff publish or override
- **When** the change is committed
- **Then** the operation MUST route through the pricing Slice 5 multi-approver workflow (single engine) with Rating's validators run fail-closed, per manifest §4.1
- **And** MUST emit auditable events with actor, before/after references, and effective times

**3. Resilience**
- **Given** transient downstream read-model lag
- **When** evaluation cannot read a consistent snapshot
- **Then** evaluation MUST fail safe (no partial pricing)
- **And** retries MUST be idempotent

## 13. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| OSS / AMS (tenant identity & hierarchy) | `tenantId`, delegation proofs, OrgTier commercial projection targets | `p1` |
| Pricing (Product Catalog) | Published `skuId`, `planId`, `priceId`, `PriceWindow`, `PriceOverlay`, `CatalogVersion`; owns PriceWindow store/state-machine/activation + `PriceWindow*` events (D-03); schedule-change events | `p1` |
| OSS metering / Rating (usage dimension population) | `dimensionKey` values on each UsageRecord; normalized usage quantity (values NOT produced here — declared/frozen here) | `p1` |
| **Usage-collector emission surface (UC1 — launch-gating)** | The built v1 collector is **pull-only** (sync REST + eventually consistent Query SPI, no accepted-order cursor, no freshness bound) — the durable ordered transport pipeline slice 12 presumes **does not exist yet**; a phase-2 Usage Event Feed — durable outbox emission with per-tenant accepted-order cursors and ingestion watermarks, additive under collector ADR-0006 — is the remedy **this PRD commits to**, to be authored in the usage-collector design set; it **gates slice-12 implementation** (SEAMS §J UC1, CRIT) | `p1` |
| Contracts & Agreements | Account-specific price terms, commitments, true-up clauses, anti-drift cap policy | `p1` |
| Subscriptions | Effective-dated Plan/Add-on links, subscription state, plan phases, `(changeEffectiveAt, changeMode)` | `p1` |
| ~~Rating & Charging (downstream gear)~~ | **Not a dependency — intra-gear since ADR-0002 / T-D-16.** The Usage → RatedCharge pipeline, dedup, windowed `Q`, unit synthesis, rated-output persistence and Billing handoff are **this gear's own** design slices 12–16 (authored 2026-07-15); the core↔pipeline contract is in-process, not an external integration. The upstream `PRD-rating-engine-202604031200` is legacy provenance (§2.2), not a live counterpart | — |
| Billing & Invoicing | Supplies `periodState`; consumes billable items + snapshots; posts immutable invoices; executes floor/cap and rounding | `p1` |
| Finance (FX) | FX rate tables and lock policies; `fxTableVersion` | `p1` |
| Promotions / Discounts | Published Coupon definitions, redemption state, campaign stacking links (TBD PRD) | `p2` |
| Spend control / credit risk | Billing (post-aggregation cap) + OSS/Policy (real-time stop) + Finance (credit risk / prepaid gating); Rating sets amount only, no enforcement | `p2` |
| BSS Architecture Manifest | §4.1 Catalog, §4.2 Rating, §4.4 Billing, §2.1.3 identities, §8 data model | `p1` |

## 14. Assumptions

- NFR targets are working assumptions (baselines from `PRD-metering-pricing-module-202601120119`) pending the program NFR workshop; capacity planning uses them until committed.
- rating-core is a pure, I/O-free crate within the one `rating` gear deployable (ADR-0002 / T-D-16), not a separate service; the earlier "logical module within the BSS Rating domain" manifest §4.2 note is superseded.
- The windowed `Q` is materialized and owned by the rating pipeline's `QMaterializer` over `windowed_counter` (Design slice 13; single writer per `(subscription, meter, dimensionKey, window)`); rating-core receives `Q` as a frozen input.
- OSS metering will emit `dimensionKey` values on usage; until then `dimensionKey` is the empty tuple and per-combination meters are the only workaround.
- Catalog/Contracts supply `glCode`/SSP/PO and FX policy pointers as frozen inputs; Rating consumes, never recomputes, supplied evidence.
- Promotions will provide a frozen coupon snapshot contract before production coupon rating; until then §17.2 is the Rating-side stub.

## 15. Open Questions

| **Question** | **Owner** | **Target Date** | **Answer** | **Date Answered** |
|--------------|-----------|-----------------|------------|-------------------|
| Numeric SLOs (p95/p99, max RPS per partition) for the pricing hot path | Program NFR workshop | TBD | Working assumption: p95 <= 100 ms per catalog lookup; p95 < 1 s overall rating path; >= 10M events/day/region. Final acceptance at NFR workshop. | — |
| Default anti-drift cap (`maxCumulativeMarkup`) value and clamp-vs-fail behavior across partner→reseller→customer | Program / Finance workshop | TBD | Step 4 is normative — a material multi-link chain MUST fail-closed at publish without a configured cap; only the default cap value and clamp-vs-hard-fail mode remain open. Single-link/non-material overlays MAY warn. | — |
| Non-negative resolved price: clamp-to-zero vs emit-as-credit | Finance | TBD | §6.1 guard is normative; only the residual-handling policy is deferred. | — |
| Follow-on capabilities (percentage, min/cap per period, bilateral, two-dimensional) | Program workshop | TBD | Prioritize after Design lock for current Scope; see §17.4. Dimensional, CAPACITY/reservation, and composite meter are in Scope. | — |
| Promotions PRD field names and coupon snapshot event contract | Promotions + Design | TBD | Align with §17.2 before production coupon rating; Rating-side semantics are normative here. | — |
| **Plan-scoped coupon evaluation base** — which component holds a plan's line set for an `AnchorPeriod`, what triggers it (it cannot precede the last member line's rating, yet an open period keeps re-resolving usage), and how a member line's re-rate cascades into the plan-scoped discount and its split-back | Promotions + Rating Design | Before `line_total` coupons go live | **Open (T-D-22, 2026-07-28)**: step 7 is per-line and every evaluation unit is sub-plan, so no such point exists today. Launch posture: `applyScope = line_total` **fails closed**, T-D-20's cross-scope exclusivity is inert, and per-line `exclusive_best` over `usage`/`recurring` is unaffected. | — |
| Formal confirmation of rating-core deployment model | Architecture / Program leadership | Before Design lock | **Superseded by ADR-0002 / T-D-16 (§14)**: rating-core is a pure crate inside the one `rating` gear deployable — the earlier "submodule of Rating vs standalone service" framing predates the consolidation (post-rename it read "submodule of itself"). Executive ack of ADR-0002 is the remaining formality. | — |
| Minimal cloud subset for a real S3 / VM / Disks catalog | PM Team | 2026-06-11 | Resolved: Dimensional and CAPACITY/reservation (consumption + capacity flavor) in Scope; **Composite meter is in launch** — the pricing gear delivers the derived-meter primitive (Slice 10, formula-as-data over ≥2 published units) and hands Rating the eval math (SEAMS.md M5, 2026-07-10); VM MAY also be priced via the instance-type dimension. (The prior Follow-on status assumed no upstream primitive; superseded.) | 2026-06-11 |
| Usage dimension-population contract (emission of `dimensionKey` values, field shapes, normalization) | OSS / Constructor Fabric Core (emission); Rating (declare/freeze) | TBD | BSS side closeable now (declare + freeze; Rating passes through). External dependency / critical path: the OSS metering emission shape. Until OSS emits values, `dimensionKey` stays empty. | — |
| (Finance) Launch without a hard spend cap / real-time spend stop — accepted? Owner of credit risk + prepaid gating | Finance | TBD | Rating owns no enforcement. Finance MUST accept launch without a ceiling, or name the gating owner (Billing post-aggregation cap / OSS-Policy real-time stop). | — |
| (Product + OSS/Policy) Free-tier level: per-meter $0 band vs per-account-per-service allowance; boundary behavior and enforcing domain | Product + OSS/Policy | TBD | Current Scope = per-`(meter, dimensionKey)` $0 band; cross-account allowance is a new aggregate (Follow-on). | — |
| (Product + Finance) Per-resource minimum charge and stance on rapid create/delete churn | Product + Finance | TBD | `minimumCharge` MAY be configured per resource; churn policy undecided. | — |
| (Finance + Legal/Tax) "Discount vs tax" ordering per jurisdiction, and whether a contractual floor claws back coupon discount | Finance + Legal/Tax | TBD | Rating emits discount lineage for Billing/Tax; default proposal = floor compares post-coupon total. | — |
| (Operations / Portal) Owner of real-time consumption visibility + budget/limit alerts | Operations / Portal | TBD | Not a Rating requirement; name the Billing/Portal owner. | — |

## 16. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Usage dimension contract slips (OSS emission) | `dimensionKey` stays empty; per-combination meters explode catalog cardinality; S3/VM cannot be billed by dimension | Lock the BSS-side dimension contract now; raise OSS emission shape as an upstream Usage Collector requirement (critical path) — §17.3 |
| **Usage-collector transport gap (UC1) is not closed before implementation** | The entire ingestion half (slices 12–13) has no durable ordered source: a poll bridge misses late-accepted records and deactivations → wrong charges; slice-12 build is blocked | Author and adopt the phase-2 Usage Event Feed in the usage-collector design set (outbox emission, per-tenant ordering, idempotency-tuple keys); track as a launch-gating dependency on the program board (SEAMS §J UC1, CRIT) |
| rating-core deployment reversed to standalone service | Manifest contradiction; integration rework | ADR-0002 / T-D-16 is the decided model (a pure crate inside the one `rating` gear); a reversal reopens the ADR, not this row — kept only to track the pending executive ack |
| Uncommitted NFR numbers (p95, throughput) | Blocks engineering capacity planning | Commit working-assumption NFRs at the program workshop before Design lock (§7.1, §15) |
| Missing anti-drift cap on material multi-link chains | Unbounded markup compounding across the channel | Step 4 fail-closed at publish without a cap; Finance-set default; clamp/fail mode decision (§15) |
| ~~`PRD-rating-engine-202604031200` draft/empty~~ — **closed by ADR-0002 / T-D-16** | — | There is no external integration contract to define: the pipeline scope that PRD stubbed was absorbed into this gear and authored as design slices 12–16 (2026-07-15). Row kept as a historical record |
| Coupon snapshot contract undefined (no Promotions PRD) | Non-reproducible coupon rating | Treat §17.2 as the Rating-side stub; align field names/events before production coupon rating |

## 17. Reference Materials

| **Material** | **Link** | **Comments** |
|--------------|----------|--------------|
| Pricing PRD (**pricing** gear — the catalog SoR this gear evaluates over) | `gears/bss/pricing/docs/PRD.md` | Scope key, `PriceWindow`, `PriceOverlay`, `modelKind` → formula mapping, the enums adopted verbatim |
| Subscriptions PRD (**subscriptions** gear) | `gears/bss/subscriptions/docs/PRD.md` | Phase/eligibility inputs, seat count, `(changeEffectiveAt, changeMode)` |
| Usage Collector — phase-2 Usage Event Feed | *No document — a commitment this PRD makes* (§13 dependency row, §16 risk row, SEAMS §J UC1) | The UC1 ingestion transport this gear's slice 12 depends on (launch-gating); to be authored in the usage-collector design set before slice 12 is built |
| Trace chain | `AGENTS.md` (repository root) | Manifest → PRD → ADR → Design → Stories |
| BSS Architecture Manifest | `docs/bss/manifest/vz-arch-manifest-bss-only.md` | **Historical — not vendored into this repo**; §4.1 Catalog, §4.2 Rating, §2.1.3 identities |
| Project glossary | `docs/project-glossary.md` | **Historical — not vendored**; canonical terms |
| Metering & pricing predecessor | `docs/bss/prd/PRD-metering-pricing-module-202601120119/…` | **Historical — not vendored**; NFR baselines, pricing-hierarchy scope migrated here (§2.2) |
| Usage-based pricing platforms (benchmark) | Metronome, Lago, OpenMeter | Reference for cloud model coverage and scope sequencing (dimensional, composite, capacity/reservation) — §17.3 |

### 17.1 Rule Evaluation Order (normative appendix, steps 1-9)

For any evaluation at timestamp `t` (UTC) and context `ctx`:

1. **Subscription composition**: Resolve active `planId`/`skuId` links and **plan phase** (trial / intro / evergreen or successor phases per Subscriptions SoR) effective at `t`. Phase selects the applicable price schedule within the plan.
2. **Base catalog row**: Select `Price`/`PriceWindow` such that `t in [effectiveFrom, effectiveTo)` on the pricing 8-axis canonical scope key `(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)` per the non-overlap invariant (manifest §4.1). Apply `priceEligibility` in class order `existing_grandfathered > new_subscriptions_only > all_subscriptions`: `new_subscriptions_only` excludes subscriptions with `activatedAt` before window `effectiveFrom`; `existing_grandfathered` includes only subscriptions activated before cutover, and within it the generation is selected by the `cohort` of the subscription's pinned price id in `pricingSnapshotRef` (never `activatedAt` alone). If no eligible window matches, evaluation MUST fail (no silent fallback). Native multi-currency: when invoice currency equals the row's price currency, skip step 8 FX.
3. **Meter mapping and billing granularity**: Map `UsageRecord` to a charge line keyed by `(meter, dimensionKey)` — the mapping MUST be injective on `(meter, dimensionKey)` per plan revision, or reject as a configuration error (fail-closed). A plan with no declared dimensions uses the empty `dimensionKey`. `billingGranularity` round-up MUST be applied to the aggregated/merged measure of the evaluation unit, never per raw `UsageRecord`. For continuous-duration meters, contiguous usage MUST be merged into a session/window measure first, then rounded up once; for discrete-count / `per_event` meters, the unit is the event; for windowed tier/volume models, round-up applies to the window measure before tier placement. The merge/aggregation is owned by the **rating pipeline** (slice 13; single-writer per `(subscription, meter, dimensionKey, window)`); **rating-core** prices the normalized aggregate. For `tierAggregationWindow != per_event`, tier/volume math MUST be evaluated over the window-aggregated quantity `Q`. When a §6.11 boundary (mid-cycle activation, plan change, phase conversion) splits an open aggregation window, each sub-window slice prices its own attributed quantity with a band offset equal to the accumulated prior-slice `Q` (tier-counter continuity, pricing `inst-tb-window-continuity`): graduated places marginally from the offset; volume selects the band by the **window total** (every slice re-resolves to the final total's band — never its own partial cumulative); package counts blocks once over the window by cumulative ceil-diff.
4. **Partner / OrgTier / brand / region overlays**: For each candidate `PriceOverlay`, apply the scope filter (§PriceOverlay scope mapping below), then apply all survivors as a sequential stack in a deterministic total order: ascending `precedence` (lower first); cross-class ties resolve by the pricing class-specificity order `customerGroup > partner > orgTier > brand > region > global` (adopted verbatim), with ascending `priceOverlayId` as the final within-class stable tie-break. This layer stacks (applies all survivors); the class order breaks ties, it does not pick a single winner. Equal `precedence` among lists with overlapping scope within one class MUST be rejected at publish (fail-closed); the class order + `priceOverlayId` tie-break is a runtime safety net. Bounded composition: the cumulative markup/discount across the full partner → reseller → customer overlay chain MUST be bounded by a configured cap (`maxCumulativeMarkup`); exceeding it MUST clamp and record (or fail-closed if hard). A material multi-link chain without a configured cap MUST fail-closed at publish.
5. **Customer / contract overlay**: Apply contract/account-level overrides after step 4, bounded by entitlement and approval rules. Contract terms outrank partner lists (Contract > Partner price overlays > Catalog base). Overrides MUST NOT introduce metering dimensions absent from the published Plan/SKU revision (publish validation rejects fail-closed).
6. **Commitment rules**: Apply drawdown/overage per contract over an ordered list of commitment pools (`commitmentPools[]`, Contracts SoR). Commitment is always evaluated at step 6 (no reordering knob). When `reservationMatch` is present, the reserved/covered portion is determined first and excluded from pool drawdown **and from the on-demand tier counter** (the remainder re-bands from zero; the in-commit pool quantity is not excluded); the remaining quantity draws down `commitmentPools[]` (waterfall); residual beyond all pools is overage / on-demand, priced per the **frozen `overageRate` selector** (T-D-19: present ⇒ that flat frozen rate, no band math on the residual; absent ⇒ banded on-demand rates placed over the **post-reservation remainder `Q`** — reservation excluded first, pool draws never deducted). In-commit billability follows the pool's frozen `poolType` (`prepaid_drawdown` due-zero vs `committed_rate` in-arrears — §6.2). The frozen pool set, `poolType`, balances @ `balanceVersion`, draw order, rollover policy, and reserved-vs-pool split MUST be carried in `pricingSnapshotRef`.
7. **Coupon overlay (Promotions)**: Apply eligible Coupon adjustments with `settlementCurrency = price` on the post-commitment line amount in price currency. Default stacking: `exclusive_best`. Record applied coupon id(s) and pre-/post-discount amounts.
8. **FX policy (rating-core abstraction)**: If invoice currency != price currency, rating-core MUST apply the FX table per policy (inputs from Finance); no implicit/provider-default FX without a policy record. Two policies: (a) per-window rate-lock (final at event time); (b) invoice-period FX (provisional amount on the hot path; re-rate by delta at period close — close-time `fxTableVersion` authoritative). Then apply coupons with `settlementCurrency = billing` to the billing-currency amount (same `fxTableVersion`).
9. **Emit monetary amounts (rating-core → Billing boundary)**: rating-core MUST emit amounts with precision sufficient for Billing; invoice rounding (per-line vs per-invoice) is applied by Billing, not rating-core. rating-core records the rounding policy id. The resolved per-line amount MUST NOT be negative before period-level phases.

> **Reserved-capacity component**: when `reservationMatch` is present, a reserved-capacity charge is evaluated at step 6 in one of two flavors: (a) consumption-flavor (matched usage at reserved rate, remainder on-demand); (b) capacity-flavor (`capacityCharge` on allocated `reservedQuantity` regardless of usage). The reserved portion is excluded from `commitmentPools[]`. Flavor and `reservedQuantity` frozen in `pricingSnapshotRef`.

> **Period-level phase (outside the per-line order)**: floor / cap per period are applied after step 9, over the period aggregate, by Billing (§17.2). Steps 1-9 are per-line and have no slot for period-level min/max.

#### PriceOverlay scope mapping (used in step 4)

| **`PriceOverlay.scope`** | **MUST match (evaluation context)** |
|-----------------------|-------------------------------------|
| `global` | Always eligible (subject to plan/SKU applicability) |
| `customerGroup` | Payer's BSS-resolved customer group at `t` (from authenticated caller claims); most-specific class |
| `partner`, `orgTier` | `sellerTenantId` (channel/reseller that sold the subscription) |
| `brand` | Plan/SKU `brandId` at `t` |
| `region` | Usage or price-row `region` key |

Tenant axes NOT used as `PriceOverlay.scope` filters: `resourceTenantId` (usage tenancy; MUST NOT alone match partner/orgTier rows); `payerTenantId` / `accountId` (contract/account overlays in step 5); `sellerTenantId` (used for `scope(partner|orgTier)`).

#### Determinism and Rating compatibility (preserved)

- **Pure function core**: determinism stated over the evaluation unit; for windowed models the window-aggregated `Q` for `(subscription, meter, dimensionKey, window)`. Given frozen inputs, the monetary outcome MUST be identical across replay, recompute, and cross-region batch workers.
- **Windowed `Q` ownership (single-writer)**: materialized and owned by the rating pipeline's `QMaterializer` (Design slice 13), single writer per partition key; rating-core consumes it frozen; concurrent re-resolve serializes on the partition key.
- **Non-negative resolved price**: MUST NOT go negative; clamp to zero or emit a structured credit (policy TBD).
- **Usage corrections / negative quantity**: deterministically reverse prior effect (refill pool, decrement `Q`), emit compensating deltas; never drive a line negative.
- **Snapshot carry / idempotency / delta idempotency / separation**: per §6.1.

#### Multi-currency (preserved)

- **Price currency**: currency of the `Price.amount` row selected in step 2; per-market list prices are first-class.
- **Presentment currency**: portal display FX, non-authoritative, outside rating-core; MUST be labelled estimates.
- **Billing currency**: invoice currency per payer account/contract; rating-core converts per step 8; per-window rate-lock final at event time; invoice-period FX emits provisional + re-rates by delta at close.
- **Coupons and currency**: price-currency coupons in step 7; billing-currency coupons after step 8.
- **rating-core / Finance boundary**: FX tables and lock policies owned by Finance; rating-core records `fxTableVersion` / locked-rate id.
- **rating-core / Billing boundary**: rating-core MUST NOT apply invoice rounding; Billing rounds in billing currency after conversion.

### 17.2 Boundary Contracts (coupons, floor/cap, plan-change proration)

**Coupons (Promotions boundary)** — normative order extends §17.1: Catalog base → Partner/OrgTier/brand/region (`PriceOverlay`) → Customer (contract/account) → Commitment (step 6) → Coupon (step 7) → FX (step 8) → Emit (step 9). Coupons apply after customer overlay and after commitment math; default before FX (price currency), exception after FX for `settlementCurrency = billing`. Coupon + partner discount both apply (partner in step 4, coupon in step 7). Coupon + graduated tier: default on the total line amount after tier math; `applyPerTierBand = true` applies per marginal band. Stacking: `exclusive_best` (default — largest customer benefit, others excluded; per line, widening to **one coupon per plan per period** whenever a `line_total` candidate is present — T-D-20), `ordered_stack` (campaign-linked `stackSequence` only), incompatible pairs fail-closed at redemption bind. Consumption contract (Rating ← Promotions): a frozen coupon snapshot with at minimum `couponId`, `adjustmentType` (percent \| fixed_amount), `value`, `settlementCurrency` (price \| billing), `applyPerTierBand`, `applyScope` (`usage` \| `recurring` \| `line_total`), `stackSequence` (required under `ordered_stack`), validity, applicability filters, redemption eligibility. `line_total` is the **authoring-side** default Promotions writes — Rating never applies it as a fallback: a snapshot *arriving* without `applyScope` (or without `stackSequence` under `ordered_stack`) MUST fail-closed. **At launch `line_total` itself fails closed** (T-D-22 — no plan-scoped evaluation base; §6.8), so launch coupons MUST be authored with an explicit `usage` or `recurring` scope.

**Period-level floor and cap** — period-level phases over the aggregated total; applied after step 9 by Billing. Attach to the usage component by default, or recurring+usage if plan-level (frozen in `pricingSnapshotRef`). Set in price currency, converted with the same FX policy/`fxTableVersion` as step 8 (billing-currency floor/cap compared after conversion; currency explicit, no implicit default). Rating sets the amount/currency/scope and emits `PeriodFloorCapObligation`; Billing executes `max(total, floor)` / `min(total, cap)`. The non-negative guard applies before floor/cap; a floor MUST NOT mask a negative line. Whether a contractual minimum-spend floor claws back coupon discount is unresolved (default proposal: floor compares post-coupon total) — §15.

**Plan-change proration** — Subscriptions owns WHEN and the up/down asymmetry policy (cross-PRD); Rating owns the evaluation semantics and consumes `(changeEffectiveAt, changeMode)`. On a plan change at `changeEffectiveAt`, rate planA over `[periodStart, changeEffectiveAt)` and planB over `[changeEffectiveAt, periodEnd)` (half-open, UTC), each against its own revision and snapshot, at full precision (Billing aggregates). Recurring component prorated on the configured `prorationBasis`. Tier `Q` continuity across the boundary per the target plan's snapshot-frozen `usageCounterOnPlanChange` (default reset unless marked carry; `carry` per unit-matched shared line only — pricing D-113 / T-D-29); commitment pools default reset (pricing publishes no pool flag). Prorated corrections to an already-rated portion emitted as deltas via the Adjustment path. Rating consumes `changeMode` to pick the split point; the policy that sets the mode is Subscriptions.

### 17.3 Cloud Catalog Readiness and Phasing

The cloud-defining models for a genuine S3 + VM + Disks catalog that are in Scope: **Dimensional pricing** and **CAPACITY / reservation pricing** (consumption- and capacity-flavor), plus the **usage dimension-population contract**. **Composite meter is in launch** — the pricing gear provides the derived-meter primitive (Slice 10) and Rating evaluates the formula-as-data; VM MAY also be priced via the instance-type dimension. The engine seams (`dimensionKey`, `reservationMatch` + `capacityCharge`, `commitmentPools[]`, `maxCumulativeMarkup`) admit further models additively — no change to the published snapshot/Rating contract.

| **Item** | **Scope** | **Unlocks** | **Hard precondition (owner)** |
|----------|-----------|-------------|-------------------------------|
| Dimensional pricing | Scope | S3 by storage-class / region / operation; VM by instance type | OSS metering emission of dimension values (external; BSS declare/freeze + Rating pass-through owned here) — critical path |
| CAPACITY / reservation pricing | Scope | Provisioned Disks / IOPS, RI-style commitments | `reservationMatch` entitlement source (OSS / Contracts) |
| Composite meter | **In launch** | VM = vCPU + RAM as one priced line | Derived-meter primitive delivered by pricing Slice 10 |

**Sequencing**: (1) lock the BSS-side dimension contract (Rating declares + freezes; Rating passes `dimensionKey` through) and raise the OSS metering emission shape as an upstream requirement to the Usage Collector PRD — the external emission is the critical path and blocks Dimensional pricing. (2) Dimensional → (3) CAPACITY/reservation. Composite meter is in launch — the pricing gear delivers the derived-meter primitive (Slice 10), so no upstream wait applies. **Risk if the dimension contract slips**: `dimensionKey` stays the empty tuple and dimension combinations can only be expressed by minting a separate meter per combination — exploding catalog cardinality.

### 17.4 Future Scope

**Tariff semantics — formulas and computation**

| **Capability** | **Priority** | **Status** | **Notes** |
|----------------|--------------|------------|-----------|
| Percentage pricing (% of base amount) | `p2` | Follow-on | Marketplace/payments; new model row in Design |
| Bounded override composition (anti-drift caps) | `p2` | Follow-on | Contract defined now (`maxCumulativeMarkup` on the overlay chain, step 4); rich policy object phased |
| Minimum fee (floor) per period | `p2` | Follow-on | Boundary/contract defined now (§17.2); Rating sets amount, Billing executes; impl phased |
| Cap (ceiling) per period | `p2` | Follow-on | Boundary/contract defined now (§17.2); bill-shock protection executed by Billing post-aggregation; impl phased |
| Two-dimensional pricing (seats x usage) | `p2` | Follow-on | Multiple meters + hybrid model; Subscriptions seat count input |
| Meter aggregation functions `last` / `unique` | `p2` | Follow-on | **`peak` and `time_weighted` are in launch** (pricing D-44 / T-D-17, §6.2 `fr-level-aggregation`) — this row previously read "beyond `sum` (peak / last / unique)" and contradicted that; corrected 2026-07-28. Composite/derived-meter **input** derivation stays window-`sum` only, and non-`sum` does not co-occur with composite at launch (§6.7); `last` / `unique` remain phased |
| Non-negative price after stacked discounts | `p3` | Deferred | Guard is normative (§6.1); only the clamp-vs-credit policy is deferred to Finance workshop |

**Plan structure and effective dating**

| **Capability** | **Priority** | **Status** | **Notes** |
|----------------|--------------|------------|-----------|
| Extended multi-SLA tier packs (beyond manifest `PlanTier`) | `p2` | Follow-on | Current Scope uses `PlanTier` only; full tier bundles with per-tier SLA packs |
| Plan change policy (immediate vs end-of-term, asymmetric up/down) | `p2` | Cross-PRD | WHEN/asymmetry owned by Subscriptions; Rating proration semantics defined now (§17.2, AC 17) |

**Commitments and reservations**

| **Capability** | **Priority** | **Status** | **Notes** |
|----------------|--------------|------------|-----------|
| Commitment rollover (burn vs carry) | `p2` | Follow-on | Per-pool policy on `commitmentPools[]` (step 6); additive |
| Multi-pool waterfall drawdown | `p2` | Follow-on | Enterprise contracts; additive over the ordered `commitmentPools[]` waterfall |
| Free tier as structural concept | `p2` | Follow-on | Current Scope expresses free only as a per-`(meter, dimensionKey)` $0 band; cross-account allowance is a new aggregate |
| Multi-year ramp, convertible RI, sustained-use auto-discount | `p3` | Deferred | Enterprise/cloud advanced |

**Cloud-specific models**

| **Capability** | **Priority** | **Status** | **Notes** |
|----------------|--------------|------------|-----------|
| Bilateral pricing (source x destination) | `p2` | Follow-on | `(source, destination)` as `dimensionKey`; consistent with the injective `(meter, dimensionKey)` rule |
| BYOL / license-attached discount | `p2` | Cross-PRD | Entitlement in OSS/Contracts; Rating consumes license flag in ctx |
| Retroactive volume tier on monthly accumulation | `p2` | Follow-on | Batch re-rate at period close; open-window late-arrival semantics defined now (AC 10) |
| Burstable credits, storage tier transitions, spot pricing | `p3` | Deferred | Cloud provider advanced |

---

*Child artifacts: ADR(s) for precedence conflicts and snapshot versioning strategy; DESIGN for Rating / rating-core ↔ Rating / Finance FX integration contracts and evaluation traces.*
