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

# PRD — Tariffs — Commercial Pricing Logic

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
  - [6.1 Deterministic Tariff Evaluation](#61-deterministic-tariff-evaluation)
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
  - [Tariff resolution and determinism](#tariff-resolution-and-determinism)
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

**Tariffs** is the BSS capability that resolves **effective commercial prices and charge formulas** deterministically for subscriptions and usage meters in a **multi-tenant hierarchy** (platform owner → channel partner / reseller → end customer), under **usage-based and hybrid commercial models**, with **financial-grade auditability**, **scalable evaluation**, and **byte-for-byte reproducible** outputs for downstream **Rating**. **Tariff evaluation** (§6.3 / §17.1) emits a **resolved tariff outcome** plus a `pricingSnapshotRef`.

It owns commercial price rules, pricing models, the override hierarchy, effective dating, snapshots, and operator controls. It does **not** compute tax, recognize revenue, manage coupon lifecycle, or enforce spend — those remain in their owning domains (§5.2).

### 1.2 Background / Problem Statement

BSS must monetize a real IaaS catalog (S3, VM, Disks) across a partner/reseller hierarchy with usage-based, hybrid, and committed-usage models. Without explicit, normative pricing semantics, rating batches, replay, and late-arrival handling diverge, and partner/customer override precedence becomes ad hoc — producing non-reproducible charges, disputes, and non-auditable financials.

This PRD fixes the **formula semantics** (flat, tiered, volume, hybrid, committed-usage), the **deterministic evaluation order**, the **multi-currency** separation (price vs billing currency vs FX policy), and **CFO-grade controls** (rule-version audit, UTC effective dating, segregation of duties on publish, ASC 606-compatible tagging) so Design implements — not invents — these rules.

Industry alignment: usage-based pricing platforms (Metronome, Lago, OpenMeter) are the coverage/sequencing benchmark for cloud model breadth (dimensional, composite, capacity/reservation); partner economics require explicit precedence integers on price lists; retroactivity never mutates posted invoices and uses delta adjustments with lineage (manifest §4.2).

### 1.3 Goals (Business Outcomes)

- **Determinism**: given frozen inputs `(window-aggregated inputs, pricingSnapshotRef, fxTableVersion)`, the monetary outcome is identical across replay, recompute, and cross-region batch workers; all divergences without input change are defects.
- **Model coverage**: flat, tiered (graduated), volume (block, two variants), hybrid (recurring + usage), and committed-usage (drawdown + overage + true-up) are supported with explicit, configured semantics.
- **Multi-currency correctness**: price currency, invoice/settlement currency, and FX policy (rate-lock per window or invoice-period FX) are separated; no implicit provider-default FX.
- **Auditability**: rule-version audit trail, UTC effective dating, two-person rule on material publish, and ASC 606-compatible allocation inputs (PO tags, SSP pointers) carried as references — not a recognition engine.
- **Scale**: horizontal evaluation per tenant/partition with bounded p95 latency and no cross-partition locks on the hot path (working-assumption targets in §7.1 until NFR workshop).

### 1.4 Glossary

| **Term** | **Definition** |
|----------|----------------|
| **Tariff** | A versioned commercial rule set binding metering dimensions to a **pricing model** and **evaluation policy** for a Plan/Price row or overlay. Persisted via Catalog + contract overlays + snapshot refs (see Design for entity mapping). |
| **Resolved tariff outcome** | The output of one **tariff evaluation**: effective rates, pricing model kind, tier thresholds, overlay winners, and snapshot identifiers — not a separate Catalog entity. |
| **Evaluation context** | Inputs to resolve one price outcome: tenant axes (`resourceTenantId`, `payerTenantId`, `sellerTenantId`), subscription/plan linkage, **subscription phase**, **`planTier`**, SKU/meter, quantity or time slice (after **billing granularity** normalization), **`tierAggregationWindow`** policy, timestamp `t` (UTC), currency/region/brand scope, **`periodState`** (`open` \| `closed_posted`, from Billing), optional **`reservationMatch`**, optional **`changeEffectiveAt` / `changeMode`**, and applicable **snapshot identifiers**. |
| **periodState** | Open/closed state of the billing period covering `t`, **supplied by Billing**. `open` → retroactive/late events may re-resolve the window and FX may be provisional; `closed_posted` → posted-period immutability applies and corrections MUST be delta-only. Required input for the retroactivity branches. |
| **reservationMatch** | Optional input describing reserved/provisioned capacity at `t`: reserved rate, reserved/allocated quantity (`reservedQuantity`), and an optional usage-coverage flag. Two charge flavors: **(a) consumption-flavor** (matched usage at reserved rate, remainder on-demand); **(b) capacity-flavor** (allocated quantity charged at reserved rate regardless of usage). Entitlement lifecycle/inventory is cross-PRD (OSS/Contracts). |
| **capacityCharge** | The capacity-flavor charge: a recurring-style charge on `reservedQuantity` (e.g. provisioned-disk GB, provisioned IOPS) at the reserved rate, emitted per period independent of usage; evaluated at step 6. |
| **Tier aggregation window** | Policy governing when tier counter `Q` resets for tiered/volume models: `calendar_month`, `invoice_period`, `subscription_lifetime`, or `per_event`. MUST be configured on the Price/plan policy and frozen in `pricingSnapshotRef`. `calendar_month` delimited in UTC; `invoice_period` anchored to the subscription billing anchor (UTC). Thresholds are half-open `[lower, upper)` — a quantity at a boundary falls in the UPPER band. |
| **Billing granularity** | Minimum billable unit for a usage price (`per_second`, `per_minute`, `per_hour`, `per_day`, or whole-unit). Usage duration/quantity rounded **up** to this unit before rate application. A per-resource `minimumCharge` MAY bound ephemeral-resource over-charge (§15). |
| **dimensionKey** | Ordered tuple of pricing-relevant event dimensions that, with `meter`, identifies one tariff line. Empty tuple = a meter with no declared dimensions. Invariant: one line per **`(meter, dimensionKey)`**. Declared dimensions come from the published Plan/SKU revision and are frozen in `pricingSnapshotRef`. Value emission on usage is an OSS/Rating contract. |
| **prorationBasis** | Day-count convention for mid-period proration: `calendar_days_actual`, `calendar_days_30`, `by_second`, or `whole_unit`. Configured on the plan/price policy and frozen in `pricingSnapshotRef`; applies to ALL mid-period proration. |
| **Price eligibility** | Who may receive a `Price`/`PriceWindow`: `all_subscriptions`, `new_subscriptions_only`, or `existing_grandfathered`. Evaluated at step 2 with subscription `activatedAt` / grandfather cutover dates. |
| **Plan phase** | Time-bounded segment of a subscription plan (trial, intro, evergreen) with its own price schedule. Structure in Subscriptions SoR; tariff evaluation resolves the active phase at `t` in step 1. |
| **CatalogVersion** | Immutable, published revision of the product catalog (Catalog SoR). One component of a pricing snapshot. |
| **pricingSnapshotRef** | Immutable **composite** reference to all frozen commercial inputs needed to reproduce a charge: at minimum `catalogVersion`, resolved price-overlay identifiers, applied coupon id(s) + stacking policy, FX lock (if any), and evaluation-policy version. **Not** equivalent to `CatalogVersion` alone. |
| **PlanTier** | Mandatory catalog attribute on every Plan/SKU. Part of evaluation context; distinct from **OrgTier** (partner commercial projection). Primary mechanism for service-tier packaging (Basic/Pro/Enterprise) in current scope. |
| **OrgTier overlay** | Partner/reseller commercial projection applied without changing AMS tenant topology (manifest §4.1). |
| **Committed usage** | Pre-purchased quantity or spend pool drawn down by metered usage; overage and true-up follow committed/overage rates. |
| **True-up obligation** | Period-end commercial adjustment surfaced as a structured `TrueUpObligation` on the evaluation result (amount, period, contract ref) for Billing — not a silent in-engine charge. |
| **Mid-cycle change** | A `PriceWindow` or overlay whose catalog `effectiveFrom` falls inside the subscriber's current invoice period (billing anchor may differ from calendar month). |
| **Retroactive pricing** | Any rule assigning a rate to usage based on a policy decision time earlier than operational processing time (late-arrival, administrative repricing). Distinct from normal effective-dated windows. |
| **PriceWindow** | Non-overlapping, UTC-bounded interval during which a `Price` row is effective. Step-2 selection key `(planId, currency, region, phase)` (extended with `priceList` when an overlay participates). The non-overlap invariant key MUST include `phase`. |
| **PriceList** | A scoped collection of price overrides with `scope(partner \| orgTier \| brand \| region \| global)` and explicit `precedence`. Eligibility resolved against evaluation-context fields before precedence stacking. |
| **Coupon** | Promotional discount instrument (id, type, validity, applicability, redemption limits, campaigns). Entity lifecycle/campaign management owned by Promotions; this PRD owns when and how an eligible coupon adjusts a resolved tariff line. |
| **Coupon stacking policy** | `exclusive_best` (default — single winning coupon) or `ordered_stack` (explicit campaign-linked sequence only). |
| **RatingRule** | Defined in manifest §4.2 — maps resolved tariff outcome to Usage → RatedCharge in Rating. Not redefined here. |
| **SSP (Standalone Selling Price)** | Price at which an entity would sell a promised good/service separately; an input to ASC 606 allocation. Carried as references on charge lines; recognition schedules are out of scope. |

## 2. Architecture Alignment

| **Field** | **Value** |
|-----------|----------|
| **Applicable Manifest(s)** | BSS |
| **Relevant Chapters** | §4.1 Product and Service Catalog; §4.2 Rating and Charging; §4.4 Billing and Invoicing (snapshot/immutability contract); §2.1.3 Multi-tenant semantics; §8 Data and Domain Model (identity invariants) |

> **Normative alignment**: extends manifest requirements for **commercial price resolution** and **deterministic rating inputs**. MUST NOT contradict: (a) Catalog as SoR for Product/SKU/Plan/Price/PriceWindow/PriceList/CatalogVersion; (b) Rating as deterministic Usage→RatedCharge→BillableItem pipeline; (c) posted financial immutability with corrections via adjustments/credit/debit notes; (d) OSS/BSS boundary (BSS MUST NOT mutate OSS topology or Policy Engine state).

> **Manifest extension (PriceWindow coverage)**: manifest §4.1 guarantees non-overlapping windows for a key; this PRD requires that key to **include `phase`** — `(planId, currency, region, phase[, priceList])` — and additionally requires **no gaps** for billable usage at `t` (if no window matches, evaluation MUST fail explicitly — AC 6).

> **PLAL deployment (normative for Design)**: **PLAL** (tariff evaluation) MUST be designed as a **logical module within the BSS Rating domain** (manifest §4.2), not a separate deployable service, unless program leadership reverses this before Design lock (manifest update required). §15 retains formal confirmation only.

### 2.1 Terminology and Naming

| **Name** | **Usage** |
|----------|-----------|
| **Tariffs** | Canonical name for this PRD's domain: commercial price rules, pricing models, override hierarchy, effective dating, snapshots, operator controls, NFR for price resolution. |
| **Tariff evaluation** | The deterministic process resolving effective commercial prices and charge formulas for a given context (§6.3). Produces a resolved tariff outcome + `pricingSnapshotRef`. |
| **Pricing Logic Abstraction Layer (PLAL)** | The implementation component that runs tariff evaluation and abstracts cross-domain inputs: Finance FX (step 8), Billing invoice rounding (step 9), Tax (out of scope). Use **PLAL** at abstraction boundaries and deployment/integration text; use **Tariffs** / **tariff evaluation** for scope and business process. |
| **Tariff Engine** | Deprecated — do not use (replaced by **Tariffs** / **tariff evaluation**). |

### 2.2 Predecessor PRDs and Scope Migration

This PRD specializes or supersedes the following scope from predecessor documents:

- **PRD-metering-pricing-module-202601120119** — "Pricing Hierarchy Orchestration (Contract > PriceList > Catalog)" and "Tiered Pricing Calculator" (P0 Rating scope) move here as §6.3 evaluation order (steps 1-9) and formula definitions. The metering/collection half remains authoritative there.
- **PRD-product-catalog-marketplace-202601120119** — "Plan & Price Modeling", "Effective Dating & Price Windows", and "Price Lists & Adjustments" define the **data primitives** this PRD evaluates; Catalog remains SoR for those primitives. Evaluation semantics and override resolution are authoritative here.
- **PRD-rating-engine-202604031200** (VHP-810) — **Status: draft / empty; integration contract TBD.** "Tiered pricing evaluation" and "Deterministic outputs + pricingSnapshotRef" are HIGH scope items there but defer formula semantics to Design. This PRD supplies those semantics; Rating remains authoritative for the Usage → RatedCharge pipeline and dedup.

## 3. Actors

### 3.1 Human Actors

#### Product Manager

**ID**: `cpt-cf-bss-tariffs-actor-product-manager`

**Role**: Defines plans, meters, tier semantics, and effective windows so commercial behavior is explicit.
**Needs**: Tariff/price-book editor, model configuration (flat/tiered/volume/hybrid/commit), UTC windows, approval submit.

#### Partner Admin

**ID**: `cpt-cf-bss-tariffs-actor-partner-admin`

**Role**: Applies scoped markups/discounts with precedence so channel economics are controlled.
**Needs**: OrgTier scope selection, adjustment stack definition, non-overlap validation, simulation.

#### Finance Analyst

**ID**: `cpt-cf-bss-tariffs-actor-finance-analyst`

**Role**: Previews invoice impacts of a future window so forecasts and ASC inputs are explainable.
**Needs**: Sample-usage profiles, candidate-window selection, evaluation-trace export.

#### Platform Operator

**ID**: `cpt-cf-bss-tariffs-actor-platform-operator`

**Role**: Owns deterministic, hierarchical tariff resolution so usage-based revenue is reproducible, auditable, and compatible with Rating and Finance controls.
**Needs**: Audit of rule versions, segregation of duties on publish, deterministic replay.

### 3.2 System Actors

#### Rating & Charging

**ID**: `cpt-cf-bss-tariffs-actor-rating`

**Role**: Consumes resolved tariff outcome + Usage; produces `RatedCharge` / `BillableItem`; owns the Usage → RatedCharge pipeline, dedup, and windowed `Q` aggregation (single-writer per `(meter, dimensionKey, window)`).

#### Billing & Invoicing

**ID**: `cpt-cf-bss-tariffs-actor-billing`

**Role**: Consumes billable items + snapshots; supplies `periodState` (open / closed_posted); posts immutable invoices; executes period-level floor/cap and invoice rounding.

#### Catalog / Price Book

**ID**: `cpt-cf-bss-tariffs-actor-catalog`

**Role**: SoR for `skuId`, `planId`, `priceId`, `PriceWindow`, `PriceList`, `CatalogVersion`; emits schedule-change events.

#### Contracts & Agreements

**ID**: `cpt-cf-bss-tariffs-actor-contracts`

**Role**: Supplies account-specific price terms, commitments, true-up clauses, and the bounded-composition cap policy.

#### Subscriptions

**ID**: `cpt-cf-bss-tariffs-actor-subscriptions`

**Role**: Owns effective-dated Plan/Add-on links, subscription state, plan phases, and the plan-change WHEN/asymmetry policy (`changeEffectiveAt`, `changeMode`).

#### Promotions / Discounts

**ID**: `cpt-cf-bss-tariffs-actor-promotions`

**Role**: Owns the Coupon entity and campaign stacking; supplies frozen coupon snapshots. Tariffs consumes; never mutates campaigns.

#### Finance (FX)

**ID**: `cpt-cf-bss-tariffs-actor-finance-fx`

**Role**: Owns FX rate tables and lock policies; PLAL consumes them as frozen inputs and records `fxTableVersion` / locked-rate id.

#### OSS / AMS (Tenant Identity)

**ID**: `cpt-cf-bss-tariffs-actor-oss-ams`

**Role**: Supplies `tenantId`, delegation proofs, and OrgTier commercial projection targets.

#### OSS Metering (Usage Dimension Population)

**ID**: `cpt-cf-bss-tariffs-actor-oss-metering`

**Role**: Emits `dimensionKey` values on each UsageRecord (e.g. S3 storage-class / region / operation; VM instance type) and normalized usage quantity. **Critical-path upstream dependency** for dimensional pricing.

## 4. Operational Concept & Environment

### 4.1 Module-Specific Environment Constraints

- **Multi-tenant isolation**: price lists and contract overrides are tenant-scoped; cross-tenant administration requires delegation proofs; a contract/account overlay MUST NOT leak across payer/seller tenant scope.
- **Time**: all effective dating and window boundaries are in **UTC**; `calendar_month` aggregation is UTC-delimited; `invoice_period` anchors to the subscription billing anchor (UTC-normalized).
- **Determinism boundary**: PLAL is a logical module within the Rating domain; it consumes frozen inputs (catalog snapshot, FX tables, coupon snapshots, windowed `Q`) and MUST NOT re-query mutable catalog state at bill-post time for posted periods.
- **Decimal precision**: PLAL emits amounts at precision sufficient for Billing; invoice rounding (per-line vs per-invoice) is applied by Billing, not PLAL. Design fixes intermediate DECIMAL precision for PLAL-emitted amounts.

**Event alignment (manifest §4.1-4.2)**:

- MUST consume: `PriceWindowScheduled`, `PriceWindowActivated`, `PriceWindowExpired`, `CatalogVersionPublished` (ordering per stream).
- MUST NOT require Rating to re-query mutable catalog state at bill-post time for posted periods; the snapshot contract remains authoritative.

> **Gating dependency (critical path for IaaS billing)**: the **usage dimension-population contract** (OSS metering → Rating → Tariffs) is the bottleneck for billing real cloud resources. The BSS side is owned here (Tariffs admits dimensions via `dimensionKey` and freezes the declared set; Rating passes them through). The external part is **OSS metering emission** of dimension values: until OSS emits them, `dimensionKey` stays the empty tuple and the only workaround is minting a separate meter per dimension combination — exploding catalog cardinality. See §17.3 and §15.

## 5. Scope

### 5.1 In Scope

| **Feature** | **Priority** | **Notes** |
|-------------|--------------|-----------|
| Deterministic tariff evaluation API (conceptual contract) for Rating | `p1` | Resolved rate/tier outcome + `pricingSnapshotRef` + metadata; replay-safe (§6.1). |
| Pricing models: flat, tiered (graduated), volume (block, A/B), hybrid, committed usage | `p1` | Formal semantics in §6.2; configurable tier boundary behavior. |
| Versioning & UTC effective dating; non-overlapping windows per manifest invariants | `p1` | Aligns with PriceWindow + PriceList; activation ordered per `(tenantId, aggregateId)`. |
| Multi-currency: price currency, conversion policy, rate-lock hooks | `p1` | PLAL applies Finance FX (step 8); no tax calculation. |
| Override hierarchy: global → partner/orgTier/brand/region → customer with explicit precedence | `p1` | §6.4 + step 4; manifest §4.1 scopes. |
| PriceList scope → tenant-axis mapping (seller/payer/brand/region) | `p1` | §6.3 / §17.1; AC 15. |
| Tier aggregation window (`Q` reset policy) for tiered/volume models | `p1` | Required on Price/plan policy; AC 14. |
| Plan phases (trial / intro / evergreen) — price resolution per active phase | `p1` | Subscriptions owns structure; step 1 + AC 16. |
| Price eligibility / grandfathering (new vs existing subscriptions) | `p1` | `priceEligibility` on PriceWindow; AC 16. |
| Billing granularity (minimum billable unit per usage price) | `p1` | Round-up before rate; step 3; AC 17. |
| Dimensional pricing — `(meter, dimensionKey)` lines | `p1` | Critical path for a real IaaS catalog; step 3 + AC 3 + AC 21. Depends on the usage dimension-population contract. |
| CAPACITY / reservation pricing (provisioned Disks/IOPS, RI-style) | `p1` | Two flavors at step 6 via `reservationMatch`: consumption (AC 22) and capacity (`capacityCharge`, AC 23). |
| Usage dimension-population contract (BSS side owned here; OSS emission external) | `p1` | Gating dependency. Tariffs declares/freezes; Rating passes `dimensionKey` through; OSS emits values (external). |
| Coupon application in tariff evaluation (order, stacking, tier/FX interaction) | `p2` | Promotions owns Coupon entity; semantics in §17.2; step 7; AC 18. |
| Mid-cycle price changes: bucket split, proration alignment to UTC cutoffs | `p2` | No posted invoice mutation. |
| Retroactive pricing modes: administrative re-rate → Adjustment deltas only | `p2` | Preserves invoice immutability; ties to Rating `ChargeAdjustment`. |
| ASC 606 alignment hooks: PO tags, SSP snapshot pointers, allocatable amount fields | `p2` | Recognition schedules remain Billing/Finance; Tariffs supplies traceable inputs. |
| Operator UX for tariff maintenance, simulation, approval thresholds | `p2` | UI screens (DESIGN, frontend). Approval workflow + audit gates are a `p1` dependency of safe evaluation (manifest §4.1 two-person rule). |

### 5.2 Out of Scope

- **API schemas, storage DDL, error code taxonomies** — Design document(s).
- **OSS metering** emission shapes and `UsageRecord` content beyond fields Rating consumes — OSS domain; Pricing consumes aggregated dimensions per the Rating contract.
- **Tax determination** and **statutory invoicing** — Tax Engine / Billing (PLAL MUST NOT compute tax). Handoff: the emitted amount MUST carry **discount lineage** (pre-/post-overlay, pre-/post-coupon amounts and applied ids) so Billing/Tax can choose gross-vs-net treatment; Tariffs supplies lineage, not ordering.
- **Full revenue recognition subledger** and **ASC 606 automated journal entries** — Finance/Billing; this PRD requires only compatible tagging and amounts.
- **Policy Engine** enforcement and **resource topology** changes — OSS.
- **Coupon / campaign lifecycle** (creation, distribution, redemption limits, fraud controls) — Promotions; Tariffs consumes frozen coupon definitions at evaluation time only.
- **Spend control and credit risk** — real-time spend stop / limit enforcement is OSS / Policy Engine; post-aggregation spend caps / bill-shock are Billing; credit risk and prepaid gating are Finance. Tariffs sets the floor/cap **amount** but performs no enforcement or gating. Launch without a hard spend ceiling requires Finance acceptance (§15).

## 6. Functional Requirements

> **Content boundary**: FRs define WHAT must be resolved (posting/evaluation semantics), not data models or APIs. Concrete schemas, proto definitions, error taxonomies, and mathematical formulas with symbol definitions are owned by the corresponding DESIGN (`DESIGN-tariffs-pricing-logic-*/`). The full deterministic step order (steps 1-9) is preserved normatively in §17.1.

### 6.1 Deterministic Tariff Evaluation

#### Deterministic evaluation API

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-deterministic-evaluation-api`

Tariff evaluation **MUST** expose a conceptual evaluation contract that, for a given evaluation context at timestamp `t` (UTC), produces a **resolved tariff outcome** (effective rates, pricing model kind, tier thresholds, overlay winners) plus a `pricingSnapshotRef` and evaluation metadata. It **MUST** be replay-safe.

**Rationale**: Rating and Finance require a stable, reproducible outcome to reproduce charges and audits.

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

#### Single outcome per frozen context (pure-function core)

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-single-outcome-determinism`

The determinism contract is stated over the **evaluation unit** (step 3): for `per_event` models a single normalized `UsageRecord`; for any model with `tierAggregationWindow != per_event`, the **window-aggregated quantity `Q`** for the `(meter, dimensionKey, window)` key. Given frozen inputs `(window-aggregated inputs, pricingSnapshotRef, fxTableVersion)`, the monetary outcome **MUST** be identical across replay, recompute, and cross-region batch workers. The windowed `Q` **MUST** be materialized and owned by the Rating `AggregationWindow` (single writer per partition key); Tariffs receives `Q` as a frozen input and **MUST NOT** aggregate. Concurrent re-resolve **MUST** serialize on the partition key.

**Rationale**: A pure-function core over frozen, window-aggregated inputs is what makes replay and late-arrival handling non-divergent without cross-partition locks.

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

#### Snapshot carry

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-snapshot-carry`

Every evaluation **MUST** emit identifiers sufficient for manifest `BillableItem.pricingSnapshotRef` and stable `{skuId, planId, priceId}`. `pricingSnapshotRef` **MUST** be a composite reference (at minimum `catalogVersion`, resolved overlay identifiers, applied coupon id(s) + stacking policy, FX lock if any, evaluation-policy version) — **not** equivalent to `CatalogVersion` alone.

**Rationale**: Reproducibility requires freezing all commercial inputs, not just the catalog version.

**Actors**: `cpt-cf-bss-tariffs-actor-billing`

#### Usage and delta idempotency

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-idempotency`

Same usage idempotency key + same snapshot **MUST NOT** double-charge (Rating dedup remains authoritative). Deltas from retroactivity / period-FX close are **new commercial events**, not the original usage key; each delta **MUST** carry a stable correction key `(window, prior-rated-version, snapshot)` so a re-rate retry is idempotent and cannot double-adjust. The owner of delta dedup (Rating or Billing) **MUST** be named in Design before the Adjustment path goes live.

**Rationale**: Deterministic replay and correction safety require distinct, stable idempotency for usage vs deltas.

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

#### Non-negative resolved price

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-non-negative-price`

A resolved per-line price **MUST NOT** go negative; evaluation **MUST** clamp to zero or emit the residual as a structured credit (clamp-vs-credit policy TBD — §15). Applies after stacked overlays, commitment, and coupons (steps 4-7) and **before** period-level floor/cap.

**Rationale**: Negative resolved lines corrupt downstream rating and revenue; a floor must not mask a negative line.

**Actors**: `cpt-cf-bss-tariffs-actor-finance-fx`

#### Separation from posted financials

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-separation`

Tariff evaluation **MUST NOT** mutate Usage or posted invoices; retroactive outcomes **MUST** flow through Adjustment paths (manifest §4.2). A correcting/negative usage event **MUST** deterministically reverse its prior commercial effect (refill drawn-down commitment pool, decrement tier counter `Q` for the affected `(meter, dimensionKey, window)`) and emit compensating deltas; it **MUST NOT** drive a resolved line negative. Correction ingestion and dedup remain Rating.

**Rationale**: Posted-financial immutability and auditable corrections are manifest invariants.

**Actors**: `cpt-cf-bss-tariffs-actor-billing`

### 6.2 Pricing Models

#### Flat pricing

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-flat-pricing`

Flat charge **MUST** be `unitPrice x Q` (or a fixed amount per period for recurring); no thresholds evaluated.

**Rationale**: The base model must be unambiguous and threshold-free.

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

#### Tiered (graduated) pricing

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-tiered-graduated`

With two or more tiers, each unit **MUST** be charged at its marginal band rate; a single tier rate **MUST NOT** be applied to all units. With one tier only, graduated and Volume Variant A are numerically identical — the distinction is by configured model kind, not by this rule. Tier counter `Q` **MUST** use the configured `tierAggregationWindow`.

**Rationale**: Graduated vs volume must be fixed in writing to prevent rating divergence.

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

#### Volume Variant A (rate on total Q)

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-volume-variant-a`

A single tier rate **MUST** apply to **all** units based on total quantity `Q` within the `tierAggregationWindow`; this variant **MUST** be configured explicitly per SKU and distinguishable from graduated.

**Rationale**: Whole-quantity pricing is a distinct commercial model and must be explicit.

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

#### Volume Variant B (block fee)

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-fr-volume-variant-b`

A flat block fee **MUST** apply for the tier block reached (not per-unit at tier rate); **MUST** be configured explicitly per SKU and distinct from Variant A.

**Rationale**: Block-fee pricing is a separate construct from per-unit volume pricing.

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

#### Hybrid pricing

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-hybrid-pricing`

Recurring and usage components **MUST** be emitted as **two distinct lines** under one `planId` (so Billing can itemize), each evaluated independently per its period boundaries. A hybrid "minimum commitment" **MUST** be expressed as committed-usage (prepaid pool + overage, step 6); a minimum monthly invoice fee (period floor) is a separate period-level construct (§6.11, §17.2) and **MUST NOT** be conflated. Attachment points: commitment (step 6) and floor/cap attach to the usage line unless the plan marks them plan-level; coupon (step 7) attaches per `applyScope` (`usage` / `recurring` → that line; `line_total` → combined total, applied once as a plan-scoped overlay and split back pro-rata across the two lines deterministically). The attachment configuration **MUST** be frozen in `pricingSnapshotRef`.

**Rationale**: Itemization, min-commit disambiguation, and deterministic coupon splitting are required for auditable hybrid plans.

**Actors**: `cpt-cf-bss-tariffs-actor-billing`

#### Committed usage

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-committed-usage`

In-commitment and overage portions **MUST** be charged at distinct rates over the ordered `commitmentPools[]` (step 6); period true-up follows the contract and **MUST** be surfaced as a structured `TrueUpObligation` (amount, period, contract ref) for Billing — not an implicit posted charge. A correcting/negative usage event **MUST** refill the drawn-down pool and emit compensating deltas; it **MUST NOT** drive the resolved line negative.

**Rationale**: Prepaid/overage economics and true-ups must be explicit and reversible.

**Actors**: `cpt-cf-bss-tariffs-actor-contracts`

### 6.3 Rule Evaluation Order

> The full normative step order (steps 1-9, plus the reserved-capacity and period-level phases) is preserved verbatim in §17.1. The FRs below carry the requirements that the order enforces.

#### Deterministic evaluation order

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-evaluation-order`

For any evaluation at `t` (UTC) and context `ctx`, the engine **MUST** apply the fixed order: (1) subscription composition + active phase, (2) base catalog row selection, (3) meter mapping + billing granularity, (4) partner/OrgTier/brand/region overlays, (5) customer/contract overlay, (6) commitment + reservation, (7) coupon, (8) FX, (9) emit. The step order is **invariant** for every contract — there is **no reordering knob**. Replay over identical inputs **MUST** be byte-identical.

**Rationale**: A single invariant order is the basis of determinism (AC 1).

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

#### Base catalog row selection

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-base-catalog-selection`

Step 2 **MUST** select `Price`/`PriceWindow` such that `t in [effectiveFrom, effectiveTo)` for `(planId, currency, region, phase)` per the non-overlap invariant, then apply `priceEligibility`. At most **one** window MUST match. If no eligible window matches, evaluation **MUST** fail (no silent fallback) for billable usage. When invoice currency equals the row's price currency, step 8 FX is skipped.

**Rationale**: Gap/overlap-free, eligibility-correct selection prevents silent mispricing.

**Actors**: `cpt-cf-bss-tariffs-actor-catalog`

#### Meter mapping and billing granularity

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-meter-mapping-granularity`

Step 3 **MUST** map `UsageRecord` to a tariff line keyed by `(meter, dimensionKey)`; the mapping **MUST** be injective on `(meter, dimensionKey)` per plan revision, or reject as a configuration error (fail-closed). `billingGranularity` round-up **MUST** be applied to the **aggregated/merged measure** of the evaluation unit, **never per raw `UsageRecord`** (twelve 5-minute samples at `per_hour` MUST bill 1 hour, not 12). The merge/aggregation is owned by Rating (single-writer per `(meter, dimensionKey, window)`); Tariffs prices the normalized aggregate.

**Rationale**: Injective mapping and aggregate-level round-up prevent line collisions and ephemeral over-charge.

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

#### PriceList scope → tenant-axis mapping

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-pricelist-scope-mapping`

Step 4 **MUST** resolve `PriceList.scope` against evaluation context: `global` always eligible; `partner` / `orgTier` match `sellerTenantId`; `brand` matches Plan/SKU `brandId` at `t`; `region` matches the usage or price-row `region` key. `resourceTenantId` **MUST NOT** alone match partner/orgTier lists; `payerTenantId` / `accountId` are used for contract/account overlays in step 5, not via `PriceList.scope`.

**Rationale**: Correct scope→axis mapping prevents cross-tenant price leakage (AC 15).

**Actors**: `cpt-cf-bss-tariffs-actor-oss-ams`

### 6.4 Override Hierarchy and Overlays

#### Overlay stacking (partner / OrgTier / brand / region)

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-overlay-stacking`

Step 4 **MUST** apply all scope-matching `PriceList` survivors as a sequential stack in a deterministic total order: ascending `precedence` (lower first), then ascending `priceListId` as the stable tie-break. This layer **stacks** (applies all survivors); it does **not** pick a single winner. Equal `precedence` among lists with overlapping scope **MUST** be rejected at publish validation (fail-closed); the `priceListId` tie-break is a runtime safety net that **MUST** still produce a single deterministic result.

**Rationale**: Deterministic stacking with fail-closed publish validation prevents undefined precedence outcomes (AC 2).

**Actors**: `cpt-cf-bss-tariffs-actor-catalog`

#### Customer / contract overlay

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-customer-contract-overlay`

Step 5 **MUST** apply contract/account-level overrides after step 4, bounded by entitlement and approval rules; contract terms outrank partner lists (Contract > Partner price lists > Catalog base). Overrides **MUST NOT** introduce metering dimensions absent from the published Plan/SKU revision; contract publish validation **MUST** reject such overlays (fail-closed). Customer-layer changes **MUST NOT** silently weaken audit controls (Contract workflow + optional Finance approval).

**Rationale**: Contract precedence and dimension integrity must hold without weakening controls.

**Actors**: `cpt-cf-bss-tariffs-actor-contracts`

#### Bounded composition (anti-drift cap)

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-fr-bounded-composition-cap`

The cumulative markup/discount across the full partner → reseller → customer overlay chain **MUST** be bounded by a configured cap (`maxCumulativeMarkup`). When the stacked result would exceed the cap, evaluation **MUST** clamp to the cap and record the clamp in metadata (or fail-closed if the cap is marked hard); it **MUST NOT** silently compound unbounded markup. For a material multi-link chain, absence of a configured cap **MUST** be fail-closed at publish (or a Finance-set default applied); a publish-time warning is acceptable only for a single-link, non-material overlay. Default cap value and clamp-vs-fail mode are a policy decision (§15).

**Rationale**: Unbounded markup compounding across the channel chain is a commercial-integrity risk.

**Actors**: `cpt-cf-bss-tariffs-actor-contracts`

### 6.5 Tier Aggregation, Eligibility, Phases, Granularity

#### Tier aggregation window

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-tier-aggregation-window`

Tiered/volume models **MUST** use the configured `tierAggregationWindow` (`calendar_month` \| `invoice_period` \| `subscription_lifetime` \| `per_event`) to govern when tier counter `Q` resets. Window boundaries: `calendar_month` in UTC; `invoice_period` anchored to the subscription billing anchor. The active value **MUST** be recorded in evaluation metadata and frozen in `pricingSnapshotRef`.

**Rationale**: Tier-counter reset policy is commercially significant and must be explicit and frozen (AC 14).

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

#### Price eligibility and grandfathering

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-price-eligibility-grandfathering`

Step 2 **MUST** apply `priceEligibility`: `new_subscriptions_only` excludes subscriptions with `activatedAt` before the window `effectiveFrom`; `existing_grandfathered` includes only subscriptions activated before cutover. If no eligible price applies, evaluation **MUST** fail (no silent fallback).

**Rationale**: New-vs-existing eligibility and grandfathering are first-class commercial rules (AC 16).

**Actors**: `cpt-cf-bss-tariffs-actor-subscriptions`

#### Plan phases

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-plan-phases`

Step 1 **MUST** resolve the active plan **phase** at `t` (trial / intro / evergreen or successor phases per Subscriptions SoR); the phase selects the applicable price schedule within the plan. Distinct phases MAY have schedules that coexist at the same `t` — this is not an overlap, since `phase` is part of the PriceWindow key.

**Rationale**: Phase-correct selection is required for intro/evergreen plans (AC 16).

**Actors**: `cpt-cf-bss-tariffs-actor-subscriptions`

#### Billing granularity round-up

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-billing-granularity`

Usage duration/quantity **MUST** be rounded **up** to the configured `billingGranularity` (`per_second` \| `per_minute` \| `per_hour` \| `per_day` \| whole-unit) before rate application, on the **merged/aggregated** measure (not per raw record). `billingGranularity` **MUST** be recorded in evaluation metadata. A per-resource `minimumCharge` MAY be configured to bound ephemeral-resource over-charge (§15).

**Rationale**: Minimum billable unit must be deterministic and applied at the aggregate (AC 17).

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

### 6.6 Commitments and Reservations

#### Commitment drawdown, overage, true-up

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-commitment-drawdown`

Step 6 **MUST** apply drawdown/overage per contract over an ordered list of commitment pools (`commitmentPools[]`, Contracts SoR), in declared order (waterfall): each pool absorbs quantity/spend up to its remaining balance before the next; residual beyond all pools is overage / on-demand. A single pool is the default special case. The frozen pool set, per-pool balances, draw order, rollover policy, and any reserved-vs-pool split **MUST** be carried in `pricingSnapshotRef`. Commitment is **always** evaluated at step 6 (no reordering).

**Rationale**: Deterministic waterfall drawdown with frozen pool state is required for reproducible committed-usage billing.

**Actors**: `cpt-cf-bss-tariffs-actor-contracts`

#### Reservation pricing — consumption-flavor

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-reservation-consumption-flavor`

When a consumption-flavor `reservationMatch` is present, the **matched portion** of measured usage **MUST** be priced at the **reserved rate** and the remainder at on-demand rates resolved in steps 2-5. The reserved portion **MUST** be excluded from `commitmentPools[]` drawdown (reservation precedes pools). The reservation-match identifier **MUST** be recorded in metadata and `pricingSnapshotRef`. With no `reservationMatch`, evaluation prices as pure usage.

**Rationale**: Reserved-rate coverage of measured usage (RI-style) must be deterministic and pool-precedent (AC 22).

**Actors**: `cpt-cf-bss-tariffs-actor-contracts`

#### Provisioned-capacity charging — capacity-flavor

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-capacity-charge`

When a capacity-flavor `reservationMatch` with `reservedQuantity` is present, evaluation **MUST** emit a `capacityCharge` = reserved rate x `reservedQuantity`, **regardless of measured usage** (zero usage still bills the allocation). The `capacityCharge` **MUST NOT** be reduced by absent usage and **MUST NOT** draw down `commitmentPools[]`. `reservedQuantity`, reserved rate, and flavor **MUST** be frozen in `pricingSnapshotRef`.

**Rationale**: Provisioned disks/IOPS bill on allocation, not consumption (AC 23).

**Actors**: `cpt-cf-bss-tariffs-actor-contracts`

### 6.7 Dimensional (Cloud) Pricing

#### Dimensional pricing — `(meter, dimensionKey)` lines

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-dimensional-pricing`

Each distinct `(meter, dimensionKey)` (e.g. S3 storage-class / region / operation; VM instance type) **MUST** resolve to its own tariff line and price, with no line collision (injective per the step-3 rule). The declared dimension set **MUST** be frozen in `pricingSnapshotRef`. A plan that declares no dimensions prices as a single empty-tuple line. A record arriving with empty or partial dimension values on a dimension-declaring plan **MUST NOT** be silently priced as a single line; evaluation **MUST** route it to an explicitly published default/catch-all line (if defined) or fail-closed (reject/quarantine) — never guess.

**Rationale**: Real IaaS catalogs require per-dimension pricing without collapsing or guessing dimensions (AC 21).

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

#### Usage dimension-population contract (BSS side)

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-dimension-population-contract`

Tariffs **MUST** own dimension **declaration** on the Plan/SKU revision and **freezing** in `pricingSnapshotRef`; Rating passes `dimensionKey` through. Value **emission** on usage is OSS metering (external upstream requirement). Until OSS emits dimension values, `dimensionKey` stays the empty tuple and per-combination meters are the only workaround (exploding cardinality — tracked as critical-path risk, §16).

**Rationale**: The BSS side of the dimension contract is closeable now; the OSS emission is the gating critical path.

**Actors**: `cpt-cf-bss-tariffs-actor-oss-metering`

### 6.8 Coupons (Promotions Overlay)

> Coupon entity lifecycle and campaign management are owned by Promotions (cross-PRD). Tariffs owns deterministic application semantics. Full placement/stacking/consumption contract preserved in §17.2.

#### Coupon application order

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-fr-coupon-application-order`

Coupons are an overlay on resolved commercial price, applied at **step 7** after steps 4-6 (post-commitment line amount). Default: `settlementCurrency = price` coupons apply in price currency before FX (step 8); `settlementCurrency = billing` coupons apply after step 8 on the billing-currency amount (same `fxTableVersion`). The applied coupon id(s) and pre-/post-discount amounts **MUST** be recorded in metadata.

**Rationale**: Deterministic coupon placement relative to overlays, commitment, and FX is required to reproduce charges (AC 18).

**Actors**: `cpt-cf-bss-tariffs-actor-promotions`

#### Coupon stacking and conflicts

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-fr-coupon-stacking`

Default stacking is `exclusive_best` (select the single coupon yielding the largest customer benefit; others MUST NOT apply on the same line). `ordered_stack` applies only when a Promotions campaign explicitly links coupons with `stackSequence` (ascending; each step uses the prior output). Campaign-marked incompatible pairs **MUST** fail-closed at redemption bind time if both would apply. A coupon snapshot omitting `applyScope` (or `stackSequence` under `ordered_stack`) **MUST** fail-closed — Tariffs **MUST NOT** infer it.

**Rationale**: Winner-takes vs ordered stacking must be unambiguous and fail-closed on missing policy (AC 18).

**Actors**: `cpt-cf-bss-tariffs-actor-promotions`

### 6.9 Multi-Currency and FX

#### Multi-currency separation

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-multi-currency`

The engine **MUST** separate **price currency** (the selected `Price.amount` row; distinct per-market rows are first-class, not FX-derived), **billing currency** (invoice currency per payer account/contract), and **presentment currency** (portal display FX, non-authoritative and outside PLAL — such amounts MUST be labelled estimates). Conversion applies only when billing currency != row currency.

**Rationale**: Conflating list price, settlement currency, and display FX causes disputes.

**Actors**: `cpt-cf-bss-tariffs-actor-finance-fx`

#### FX policy (PLAL abstraction)

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-fx-policy`

When invoice currency != price currency, PLAL **MUST** apply the FX table per Finance policy and record `fxTableVersion` or locked-rate id; it **MUST NOT** use implicit/provider-default FX without a policy record. Two deterministic policies: (a) **per-window rate-lock** — final at event time; (b) **invoice-period FX** — emit a **provisional** amount at the locked/spot rate (flagged provisional) on the hot path and **re-rate by delta at period close** via the Adjustment path (close-time `fxTableVersion` is authoritative). Replay over identical inputs (including which `fxTableVersion` applied at which stage) **MUST** be byte-identical.

**Rationale**: Explicit, recorded FX with provisional+delta close keeps the hot path fast and replay byte-identical (AC 8).

**Actors**: `cpt-cf-bss-tariffs-actor-finance-fx`

### 6.10 Retroactivity and Corrections

#### Posted-period protection

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-posted-period-protection`

When `periodState = closed_posted`, a retroactive tariff change to usage in that period **MUST NOT** alter posted invoice lines and **MUST** generate **delta** adjustments consumable by Billing per immutability rules. Retroactive runs **MUST** separately record usage-observation time and pricing-policy decision time in the audit log.

**Rationale**: Posted financials are immutable; corrections flow as auditable deltas (AC 9).

**Actors**: `cpt-cf-bss-tariffs-actor-billing`

#### Late-arriving usage into an aggregate window

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-fr-late-arriving-usage-reresolve`

For a graduated/volume model over `tierAggregationWindow != per_event` with `periodState = open`, late usage arriving after some events were rated **MUST** trigger deterministic re-resolution of tier placement for the whole window-aggregated `Q` and emit **DELTA** adjustments for already-rated events (no mutation of prior outputs). With `periodState = closed_posted`, the correction follows posted-period protection. A missing `periodState` **MUST** fail-closed (no guessing).

**Rationale**: Open-window late arrivals must re-resolve deterministically without mutating prior outputs (AC 19).

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

#### Usage corrections / negative quantity

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-fr-usage-corrections`

A correcting/negative usage event **MUST** deterministically reverse its prior commercial effect: refill the drawn-down commitment pool and decrement tier counter `Q` for the affected `(meter, dimensionKey, window)`, emitting compensating deltas. It **MUST NOT** drive a resolved line negative. Correction ingestion and dedup remain Rating.

**Rationale**: Reversals must be deterministic and non-negative to keep commitment and tier state correct.

**Actors**: `cpt-cf-bss-tariffs-actor-rating`

### 6.11 Period-Level and Plan-Change Obligations

> These are period-level phases outside the per-line step order (steps 1-9 have no slot). Full boundary contracts in §17.2.

#### Period-level floor/cap obligation

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-fr-period-floor-cap-obligation`

Minimum fee (floor) and maximum charge (cap) per period are **period-level** phases over the aggregated total, applied **after** per-line emission (step 9). Tariffs **MUST** set the floor/cap amount, currency, and attachment scope and emit a structured `PeriodFloorCapObligation` (amount, comparison basis, period, contract/plan ref); **Billing executes** it (`max(total, floor)` / `min(total, cap)`) during period aggregation. PLAL **MUST NOT** apply the min/max at line aggregation or round. The non-negative guard applies to each line **before** floor/cap; a floor **MUST NOT** mask a negative line. Whether a contractual floor claws back coupon discount is unresolved (§15; default proposal: floor compares post-coupon total).

**Rationale**: Period-level min/max must be reserved as a Billing-executed obligation, not a per-line op.

**Actors**: `cpt-cf-bss-tariffs-actor-billing`

#### Mid-cycle proration

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-fr-mid-cycle-proration`

When a `PriceWindow` activates during an invoice period, charges **MUST** be computed separately for each sub-window with distinct snapshots, each emitted at **full precision** (no invoice rounding); Billing aggregates and rounds. Any recurring component prorated across the boundary **MUST** use the configured `prorationBasis` frozen in `pricingSnapshotRef`.

**Rationale**: Mid-cycle window changes must split deterministically and defer rounding to Billing (AC 7).

**Actors**: `cpt-cf-bss-tariffs-actor-billing`

#### Plan-change proration

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-fr-plan-change-proration`

On a plan change at `changeEffectiveAt`, evaluation **MUST** rate planA over `[periodStart, changeEffectiveAt)` and planB over `[changeEffectiveAt, periodEnd)` (half-open, UTC) against each plan's own revision and snapshot, each at full precision (Billing aggregates). The recurring component **MUST** be prorated on the configured `prorationBasis`. Tier `Q` and commitment-pool carry-vs-reset across the boundary **MUST** follow snapshot-frozen configuration. Corrections to an already-rated portion **MUST** be emitted as deltas. Evaluation **MUST** consume `(changeEffectiveAt, changeMode)` and **MUST NOT** decide the change mode (Subscriptions owns the policy).

**Rationale**: Plan-change splits must be deterministic and consume — not decide — the change mode (AC 20).

**Actors**: `cpt-cf-bss-tariffs-actor-subscriptions`

### 6.12 Governance and ASC 606 Traceability

#### ASC 606 traceable identifiers

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-fr-asc606-traceable-identifiers`

The resolved tariff outcome **MUST** always include `performanceObligationRef` and `sspSnapshotPointer` fields (nullable when not applicable). Non-null values **MUST** be immutable once emitted — subsequent catalog changes **MUST NOT** alter an emitted reference. Billing/Finance MAY ignore null fields.

**Rationale**: Downstream revenue allocation requires stable, immutable PO/SSP references — not a recognition engine here (AC 10).

**Actors**: `cpt-cf-bss-tariffs-actor-billing`

#### Publish approval and audit governance

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-fr-publish-approval-governance`

Material tariff publish or override **MUST** require a multi-approver workflow for above-threshold changes (manifest §4.1 two-person rule) and **MUST** emit auditable events with actor, before/after references, and effective times. Publish validation **MUST** fail-closed on ambiguous precedence, ambiguous meter mapping, missing anti-drift cap on material chains, and contract overlays introducing undeclared dimensions.

**Rationale**: Safe tariff evaluation depends on segregation of duties and fail-closed publish gates before production.

**Actors**: `cpt-cf-bss-tariffs-actor-platform-operator`

## 7. Non-Functional Requirements

### 7.1 NFR Inclusions

> Targets below are **working assumptions** (baselines from `PRD-metering-pricing-module-202601120119`) pending the program NFR workshop; rows marked TBD MUST be committed before Design lock (§15).

#### Throughput and latency

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-nfr-throughput-latency`

Tariff evaluation **MUST** meet p95 latency targets: **< 100 ms** for catalog price lookup and **< 1 s** for the overall rating path; hot-path throughput **MUST** sustain **>= 10M events/day/region**.

**Threshold**: p95 <= 100 ms catalog lookup; p95 < 1 s overall rating path; >= 10M events/day/region (working assumption; final acceptance at NFR workshop, date TBD).

**Rationale**: Rating is on the monetization critical path; delays become revenue leakage or disputes.

#### Horizontal scale (no cross-partition locks)

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-nfr-horizontal-scale`

Horizontal scaling **MUST** avoid cross-partition locks on the evaluation hot path; ordering is per-partition `(meter, dimensionKey, window)`, not global.

**Threshold**: Zero cross-partition locks on the hot path; per-partition ordering only.

**Rationale**: Cross-partition locking caps throughput and breaks the >= 10M events/day/region target.

#### Audit completeness and segregation of duties

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-nfr-audit-segregation`

Material tariff publishes/overrides **MUST** require multi-approver workflow for above-threshold changes and **MUST** emit auditable events with actor, before/after references, and effective times.

**Threshold**: 100% of material publishes carry multi-approver sign-off and a complete before/after audit event.

**Rationale**: CFO-grade controls and partner trust require segregation of duties and complete audit.

#### Resilience (fail-safe, idempotent retries)

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-nfr-resilience`

When evaluation cannot read a consistent snapshot, it **MUST** fail safe (no partial pricing); retries **MUST** be idempotent.

**Threshold**: Zero partial/best-guess priced outputs under read-model lag; idempotent retry on transient failure.

**Rationale**: Financial correctness requires fail-closed behavior, never best-guess pricing (AC 13).

### 7.2 NFR Exclusions

Explicit dispositions for domains not owned by this PRD (no silent omissions):

- **Tax computation NFRs**: Not applicable — owned by Tax Engine / Billing; PLAL MUST NOT compute tax.
- **Revenue recognition schedule performance**: Not applicable — Finance/Billing own recognition; this PRD supplies tagging/amounts only.
- **Spend-enforcement / real-time stop latency**: Not applicable — OSS / Policy Engine (real-time stop), Billing (post-aggregation cap), Finance (credit risk); Tariffs sets the amount, performs no enforcement.
- **Frontend UX performance / accessibility (WCAG) / i18n**: Not applicable to this backend PRD — owned by the corresponding frontend DESIGN.

## 8. Five Quality Vectors Analysis

| **Quality Vector** | **Show-Stopper Requirements** | **Rationale** |
|--------------------|-------------------------------|---------------|
| **Efficiency** | Evaluation MUST be cache-friendly (read models, immutable snapshots) and avoid repeated full catalog scans per usage event. | Usage pipelines are volume-heavy; CPU/IO waste raises unit cost of goods sold for cloud metering. |
| **Reliability** | Outcomes MUST be replay-deterministic; failures MUST be explicit (fail-closed), never best-guess pricing. | Financial correctness and partner trust require reproducible charges and defensible audits. |
| **Performance** | Hot-path and batch rating MUST scale horizontally per tenant/partition with bounded p95 latency under peak OSS usage (targets in §7.1). | Rating is on the monetization critical path; delays become revenue leakage or disputes. |
| **Security** | Tenant isolation for price lists and contract overrides; delegation proofs for cross-tenant administration; immutable audit for changes. | Pricing data is commercially sensitive; cross-tenant leakage is a critical incident class. |
| **Versatility** | The model matrix (flat/tiered/volume/hybrid/commitment) and overlay hierarchy MUST extend without breaking snapshot contracts to Rating. | Channel business models evolve; rigid pricing cores force expensive parallel systems. |

## 9. Public Library Interfaces

> Tariffs is a backend pricing module (PLAL within the Rating domain), not a client library. Interfaces below are high-level contracts; concrete API schemas, endpoints, and DDL belong in DESIGN.

### 9.1 Public API Surface

#### Tariff evaluation contract

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-interface-tariff-evaluation`

**Type**: conceptual evaluation contract (shape in Design)

**Stability**: stable (contract intent), schema unstable (Design owns)

**Description**: Given an evaluation context at `t`, returns a resolved tariff outcome (rates, model kind, tier thresholds, overlay winners), `pricingSnapshotRef`, discount lineage, and evaluation metadata (applied coupons, `tierAggregationWindow`, `fxTableVersion`, granularity). Replay-safe and deterministic.

**Breaking Change Policy**: Major version bump for incompatible request/response changes; snapshot semantics are part of the contract.

### 9.2 External Integration Contracts

#### Rating handoff contract

- [ ] `p1` - **ID**: `cpt-cf-bss-tariffs-contract-rating-handoff`

**Direction**: provided by Tariffs to Rating

**Protocol/Format**: resolved tariff outcome + `pricingSnapshotRef` + obligations (`TrueUpObligation`, `PeriodFloorCapObligation`); Rating maps to RatedCharge / BillableItem (Design).

**Compatibility**: Snapshot-referenced and replay-safe; Rating owns Usage → RatedCharge pipeline, dedup, and windowed `Q`.

#### Finance FX input contract

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-contract-finance-fx-input`

**Direction**: required from Finance

**Protocol/Format**: FX rate tables and lock policies with `fxTableVersion`; per-window rate-lock and invoice-period FX modes (Design).

**Compatibility**: Immutable frozen inputs; PLAL records `fxTableVersion` / locked-rate id; no implicit provider defaults.

#### Promotions coupon snapshot contract

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-contract-promotions-coupon`

**Direction**: required from Promotions

**Protocol/Format**: frozen coupon snapshot (`couponId`, `adjustmentType`, `value`, `settlementCurrency`, `applyPerTierBand`, `applyScope`, `stackSequence`, validity, applicability, redemption eligibility) (Design).

**Compatibility**: Fail-closed on missing `applyScope` / `stackSequence` under `ordered_stack`; Tariffs never infers coupon rules from mutable campaign UI state.

#### Billing periodState / obligation contract

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-contract-billing-periodstate`

**Direction**: bidirectional with Billing

**Protocol/Format**: Billing supplies `periodState` (open / closed_posted); Tariffs emits `PeriodFloorCapObligation` and full-precision sub-window amounts; Billing aggregates, applies floor/cap, and rounds (Design).

**Compatibility**: PLAL MUST NOT round or apply period-level min/max; Billing owns aggregation and rounding policy id.

## 10. Use Cases

#### Tariff and price-book editing

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-usecase-tariff-editor`

**Actor**: `cpt-cf-bss-tariffs-actor-product-manager`

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

#### Partner price-list management

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-usecase-partner-pricelist`

**Actor**: `cpt-cf-bss-tariffs-actor-partner-admin`

**Preconditions**:
- An OrgTier / partner scope exists for the seller tenant.

**Main Flow**:
1. Select OrgTier scope.
2. Define the adjustment stack with explicit precedence.
3. Validate non-overlap and simulate against sample usage.

**Postconditions**:
- A scope-filtered `PriceList` is staged with deterministic precedence.

**Alternative Flows**:
- **Equal precedence with overlapping scope**: rejected at publish (fail-closed).

#### Finance simulation of a future window

- [ ] `p2` - **ID**: `cpt-cf-bss-tariffs-usecase-finance-simulation`

**Actor**: `cpt-cf-bss-tariffs-actor-finance-analyst`

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
| Partner price list manager | As a Partner Admin, I apply scoped markups/discounts with precedence so channel economics are controlled | 1. Select OrgTier scope<br>2. Define adjustment stack<br>3. Validate non-overlap and simulate | — |
| Finance simulation | As a Finance Analyst, I preview invoice impacts of a future window so forecasts and ASC inputs are explainable | 1. Upload/select sample usage profile<br>2. Pick candidate window<br>3. Export evaluation trace | — |

## 12. Acceptance Criteria

> **As a** platform operator **I want** deterministic, hierarchical tariff resolution **so that** usage-based revenue is reproducible, auditable, and compatible with Rating and Finance controls.

### Tariff resolution and determinism

**1. Single outcome per frozen context**
- **Given** a fixed evaluation context and frozen `pricingSnapshotRef` inputs
- **When** two workers evaluate the same usage record
- **Then** they MUST produce identical resolved unit rates and pre-tax monetary amounts before the Billing tax stage
- **And** all divergences without input change MUST be treated as defects

**2. Hierarchy application order**
- **Given** global, partner `PriceList`, and customer contract overrides that all apply to the same `planId`
- **When** evaluation resolves price at `t`
- **Then** overrides MUST apply in the order defined in §17.1, the partner-layer stack ordered by ascending `PriceList.precedence`
- **And** the evaluation MUST emit an audit trail of which layer produced the winning values
- **Given** two `PriceList` rows with overlapping scope and equal `precedence`
- **When** publish validation runs
- **Then** publish MUST be rejected (fail-closed); if such a pair reaches runtime, evaluation MUST apply the deterministic `priceListId` tie-break and MUST NOT produce an undefined result

**3. Meter ambiguity rejection**
- **Given** a plan/SKU publish where a single `(meter, dimensionKey)` maps to more than one tariff line
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

### Time, versioning, currency

**6. Effective windows**
- **Given** only non-overlapping `PriceWindow` rows for a `(planId, currency, region, phase, priceList)` key (the key includes `phase`)
- **When** time `t` is queried for base catalog selection per step 2
- **Then** at most one window MUST match
- **And** distinct phases MAY have schedules that coexist at the same `t` — not an overlap, since `phase` is part of the key
- **And** if none match, evaluation MUST fail explicitly for billable usage (no silent fallback)

**7. Mid-cycle activation**
- **Given** a `PriceWindow` activating at `effectiveFrom` during an invoice period
- **When** usage spans the boundary
- **Then** charges MUST be computed separately for each sub-window with distinct snapshots, each emitted without invoice rounding (full precision)
- **And** the invoice-period total MUST be computed by Billing as the sum of sub-window amounts followed by Billing's rounding policy — PLAL MUST NOT round at aggregation
- **And** any prorated recurring component MUST use the configured `prorationBasis` frozen in `pricingSnapshotRef`

**8. Multi-currency (PLAL FX abstraction)**
- **Given** price currency differs from invoice currency
- **When** PLAL applies conversion per step 8
- **Then** the FX table version or locked rate id MUST be recorded in the evaluation result
- **And** conversion MUST NOT use implicit provider defaults without a policy record

### Retroactivity and corrections

**9. Posted period protection**
- **Given** an invoice already posted for period `P`
- **When** a retroactive tariff change is applied to usage in `P`
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
- **Given** a tariff evaluation that produces a charge for a subscription
- **When** the result is emitted to Rating / Billing
- **Then** the resolved outcome MUST always include `performanceObligationRef` and `sspSnapshotPointer` (nullable when not applicable)
- **And** non-null values MUST be immutable once emitted — subsequent catalog changes MUST NOT alter an emitted reference

### Tier aggregation, overlays, and eligibility

**12. Tier aggregation window**
- **Given** a tiered or volume SKU with `tierAggregationWindow = invoice_period`
- **When** usage events occur in two sub-periods of the same invoice period with quantities `Q1` and `Q2`
- **Then** tier counter `Q` MUST equal `Q1 + Q2` within that invoice period (not reset per event unless `per_event`)
- **And** the active `tierAggregationWindow` value MUST be recorded in metadata and `pricingSnapshotRef`

**13. PriceList scope and tenant axes**
- **Given** a partner `PriceList` scoped to `sellerTenantId = Partner-A` and a context with `sellerTenantId = Partner-B`
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

### Plan change and proration

**17. Plan-change proration within a period**
- **Given** a subscription changes from planA to planB at `changeEffectiveAt` inside one billing period
- **When** evaluation rates the period
- **Then** it MUST rate planA over `[periodStart, changeEffectiveAt)` and planB over `[changeEffectiveAt, periodEnd)` (half-open, UTC) against each plan's own revision and snapshot
- **And** each sub-window MUST be emitted at full precision (Billing aggregates)
- **And** the recurring component MUST be prorated on the configured `prorationBasis` frozen in `pricingSnapshotRef`
- **And** tier `Q` and commitment-pool carry-vs-reset across the boundary MUST follow the snapshot-frozen configuration
- **And** corrections to an already-rated portion MUST be emitted as deltas via the Adjustment path
- **And** evaluation MUST consume `(changeEffectiveAt, changeMode)` and MUST NOT decide the change mode

### Cloud resource pricing

**18. Dimensional pricing**
- **Given** a meter with declared dimensions (e.g. S3 storage-class / region / operation) and dimension values present on the usage record
- **When** evaluation maps usage at `t` (step 3)
- **Then** each distinct `(meter, dimensionKey)` MUST resolve to its own tariff line and price, with no line collision
- **And** the declared dimension set MUST be frozen in `pricingSnapshotRef`
- **Given** a plan that declares dimensions but a record arrives with empty or partial dimension values
- **Then** the record MUST NOT be silently priced as a single line; evaluation MUST route it to an explicitly published default/catch-all line (if defined) or fail-closed — never guess

**19. Reservation pricing — consumption-flavor**
- **Given** a consumption-flavor `reservationMatch` covering part of the measured usage at `t`
- **When** evaluation runs step 6
- **Then** the matched portion MUST be priced at the reserved rate and the remainder at on-demand rates from steps 2-5
- **And** the reserved portion MUST be excluded from `commitmentPools[]` drawdown
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
- **Then** the operation MUST require a multi-approver workflow for above-threshold changes per manifest §4.1
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
| Catalog / Price Book | Published `skuId`, `planId`, `priceId`, `PriceWindow`, `PriceList`, `CatalogVersion`; schedule-change events | `p1` |
| OSS metering / Rating (usage dimension population) | `dimensionKey` values on each UsageRecord; normalized usage quantity (values NOT produced here — declared/frozen here) | `p1` |
| Contracts & Agreements | Account-specific price terms, commitments, true-up clauses, anti-drift cap policy | `p1` |
| Subscriptions | Effective-dated Plan/Add-on links, subscription state, plan phases, `(changeEffectiveAt, changeMode)` | `p1` |
| Rating & Charging | Consumes resolved tariff outcome + Usage; owns Usage → RatedCharge, dedup, windowed `Q`. Downstream PRD `PRD-rating-engine-202604031200` is draft/empty; contract TBD | `p1` |
| Billing & Invoicing | Supplies `periodState`; consumes billable items + snapshots; posts immutable invoices; executes floor/cap and rounding | `p1` |
| Finance (FX) | FX rate tables and lock policies; `fxTableVersion` | `p1` |
| Promotions / Discounts | Published Coupon definitions, redemption state, campaign stacking links (TBD PRD) | `p2` |
| Spend control / credit risk | Billing (post-aggregation cap) + OSS/Policy (real-time stop) + Finance (credit risk / prepaid gating); Tariffs sets amount only, no enforcement | `p2` |
| BSS Architecture Manifest | §4.1 Catalog, §4.2 Rating, §4.4 Billing, §2.1.3 identities, §8 data model | `p1` |

## 14. Assumptions

- NFR targets are working assumptions (baselines from `PRD-metering-pricing-module-202601120119`) pending the program NFR workshop; capacity planning uses them until committed.
- PLAL is a logical module within the BSS Rating domain (manifest §4.2), not a separate deployable service, pending executive confirmation before Design lock.
- The windowed `Q` is materialized and owned by the Rating `AggregationWindow` (single writer per `(meter, dimensionKey, window)`); Tariffs receives `Q` as a frozen input.
- OSS metering will emit `dimensionKey` values on usage; until then `dimensionKey` is the empty tuple and per-combination meters are the only workaround.
- Catalog/Contracts supply `glCode`/SSP/PO and FX policy pointers as frozen inputs; Tariffs consumes, never recomputes, supplied evidence.
- Promotions will provide a frozen coupon snapshot contract before production coupon rating; until then §17.2 is the Tariffs-side stub.

## 15. Open Questions

| **Question** | **Owner** | **Target Date** | **Answer** | **Date Answered** |
|--------------|-----------|-----------------|------------|-------------------|
| Numeric SLOs (p95/p99, max RPS per partition) for the pricing hot path | Program NFR workshop | TBD | Working assumption: p95 <= 100 ms per catalog lookup; p95 < 1 s overall rating path; >= 10M events/day/region. Final acceptance at NFR workshop. | — |
| Default anti-drift cap (`maxCumulativeMarkup`) value and clamp-vs-fail behavior across partner→reseller→customer | Program / Finance workshop | TBD | Step 4 is normative — a material multi-link chain MUST fail-closed at publish without a configured cap; only the default cap value and clamp-vs-hard-fail mode remain open. Single-link/non-material overlays MAY warn. | — |
| Non-negative resolved price: clamp-to-zero vs emit-as-credit | Finance | TBD | §6.1 guard is normative; only the residual-handling policy is deferred. | — |
| Follow-on capabilities (percentage, min/cap per period, composite meter, bilateral, two-dimensional) | Program workshop | TBD | Prioritize after Design lock for current Scope; see §17.4. Dimensional and CAPACITY/reservation are in Scope. | — |
| Promotions PRD field names and coupon snapshot event contract | Promotions + Design | TBD | Align with §17.2 before production coupon rating; Tariffs-side semantics are normative here. | — |
| Formal confirmation of PLAL deployment model (submodule of Rating vs standalone service) | Architecture / Program leadership | Before Design lock | Normative for Design: submodule of Rating. Executive confirmation pending; standalone requires manifest update. | — |
| Minimal cloud subset for a real S3 / VM / Disks catalog | PM Team | 2026-06-11 | Resolved: Dimensional and CAPACITY/reservation (consumption + capacity flavor) in Scope; Composite meter is Follow-on (blocked on a derived-meter primitive in Plan & Price / Catalog; VM priced via instance-type dimension at launch). | 2026-06-11 |
| Usage dimension-population contract (emission of `dimensionKey` values, field shapes, normalization) | OSS / CyberFabric Core (emission); Tariffs (declare/freeze) | TBD | BSS side closeable now (declare + freeze; Rating passes through). External dependency / critical path: the OSS metering emission shape. Until OSS emits values, `dimensionKey` stays empty. | — |
| (Finance) Launch without a hard spend cap / real-time spend stop — accepted? Owner of credit risk + prepaid gating | Finance | TBD | Tariffs owns no enforcement. Finance MUST accept launch without a ceiling, or name the gating owner (Billing post-aggregation cap / OSS-Policy real-time stop). | — |
| (Product + OSS/Policy) Free-tier level: per-meter $0 band vs per-account-per-service allowance; boundary behavior and enforcing domain | Product + OSS/Policy | TBD | Current Scope = per-`(meter, dimensionKey)` $0 band; cross-account allowance is a new aggregate (Follow-on). | — |
| (Product + Finance) Per-resource minimum charge and stance on rapid create/delete churn | Product + Finance | TBD | `minimumCharge` MAY be configured per resource; churn policy undecided. | — |
| (Finance + Legal/Tax) "Discount vs tax" ordering per jurisdiction, and whether a contractual floor claws back coupon discount | Finance + Legal/Tax | TBD | Tariffs emits discount lineage for Billing/Tax; default proposal = floor compares post-coupon total. | — |
| (Operations / Portal) Owner of real-time consumption visibility + budget/limit alerts | Operations / Portal | TBD | Not a Tariffs requirement; name the Billing/Portal owner. | — |

## 16. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Usage dimension contract slips (OSS emission) | `dimensionKey` stays empty; per-combination meters explode catalog cardinality; S3/VM cannot be billed by dimension | Lock the BSS-side dimension contract now; raise OSS emission shape as an upstream Usage Collector requirement (critical path) — §17.3 |
| PLAL deployment reversed to standalone service | Manifest contradiction; integration rework | Treat submodule-of-Rating as normative for Design; require executive confirmation + manifest update before reversal (§15) |
| Uncommitted NFR numbers (p95, throughput) | Blocks engineering capacity planning | Commit working-assumption NFRs at the program workshop before Design lock (§7.1, §15) |
| Missing anti-drift cap on material multi-link chains | Unbounded markup compounding across the channel | Step 4 fail-closed at publish without a cap; Finance-set default; clamp/fail mode decision (§15) |
| `PRD-rating-engine-202604031200` draft/empty | Integration contract undefined | This PRD supplies formula semantics; Rating remains authoritative for the pipeline; resolve contract before Design lock |
| Coupon snapshot contract undefined (no Promotions PRD) | Non-reproducible coupon rating | Treat §17.2 as the Tariffs-side stub; align field names/events before production coupon rating |

## 17. Reference Materials

| **Material** | **Link** | **Comments** |
|--------------|----------|--------------|
| BSS Architecture Manifest | `docs/bss/manifest/vz-arch-manifest-bss-only.md` | §4.1 Catalog, §4.2 Rating, §2.1.3 identities, §4.1 invariants |
| Project glossary | `docs/project-glossary.md` | Canonical terms |
| Trace chain | `AGENTS.md` (repository root) | Manifest → PRD → ADR → Design → Stories |
| Metering & pricing predecessor | `docs/bss/prd/PRD-metering-pricing-module-202601120119/PRD-metering-pricing-module-202601120119.md` | NFR baselines; pricing-hierarchy scope migrated here |
| Usage-based pricing platforms (benchmark) | Metronome, Lago, OpenMeter | Reference for cloud model coverage and scope sequencing (dimensional, composite, capacity/reservation) — §17.3 |

### 17.1 Rule Evaluation Order (normative appendix, steps 1-9)

For any evaluation at timestamp `t` (UTC) and context `ctx`:

1. **Subscription composition**: Resolve active `planId`/`skuId` links and **plan phase** (trial / intro / evergreen or successor phases per Subscriptions SoR) effective at `t`. Phase selects the applicable price schedule within the plan.
2. **Base catalog row**: Select `Price`/`PriceWindow` such that `t in [effectiveFrom, effectiveTo)` for `(planId, currency, region, phase)` per the non-overlap invariant (manifest §4.1). Apply `priceEligibility`: `new_subscriptions_only` excludes subscriptions with `activatedAt` before window `effectiveFrom`; `existing_grandfathered` includes only subscriptions activated before cutover. If no eligible window matches, evaluation MUST fail (no silent fallback). Native multi-currency: when invoice currency equals the row's price currency, skip step 8 FX.
3. **Meter mapping and billing granularity**: Map `UsageRecord` to a tariff line keyed by `(meter, dimensionKey)` — the mapping MUST be injective on `(meter, dimensionKey)` per plan revision, or reject as a configuration error (fail-closed). A plan with no declared dimensions uses the empty `dimensionKey`. `billingGranularity` round-up MUST be applied to the aggregated/merged measure of the evaluation unit, never per raw `UsageRecord`. For continuous-duration meters, contiguous usage MUST be merged into a session/window measure first, then rounded up once; for discrete-count / `per_event` meters, the unit is the event; for windowed tier/volume models, round-up applies to the window measure before tier placement. The merge/aggregation is owned by Rating (single-writer per `(meter, dimensionKey, window)`); Tariffs prices the normalized aggregate. For `tierAggregationWindow != per_event`, tier/volume math MUST be evaluated over the window-aggregated quantity `Q`.
4. **Partner / OrgTier / brand / region overlays**: For each candidate `PriceList`, apply the scope filter (§PriceList scope mapping below), then apply all survivors as a sequential stack in a deterministic total order: ascending `precedence` (lower first), then ascending `priceListId` as the stable tie-break. This layer stacks (applies all survivors); it does not pick a single winner. Equal `precedence` among lists with overlapping scope MUST be rejected at publish (fail-closed); the `priceListId` tie-break is a runtime safety net. Bounded composition: the cumulative markup/discount across the full partner → reseller → customer overlay chain MUST be bounded by a configured cap (`maxCumulativeMarkup`); exceeding it MUST clamp and record (or fail-closed if hard). A material multi-link chain without a configured cap MUST fail-closed at publish.
5. **Customer / contract overlay**: Apply contract/account-level overrides after step 4, bounded by entitlement and approval rules. Contract terms outrank partner lists (Contract > Partner price lists > Catalog base). Overrides MUST NOT introduce metering dimensions absent from the published Plan/SKU revision (publish validation rejects fail-closed).
6. **Commitment rules**: Apply drawdown/overage per contract over an ordered list of commitment pools (`commitmentPools[]`, Contracts SoR). Commitment is always evaluated at step 6 (no reordering knob). When `reservationMatch` is present, the reserved/covered portion is determined first and excluded from pool drawdown; the remaining quantity draws down `commitmentPools[]` (waterfall); residual beyond all pools is overage / on-demand. The frozen pool set, balances, draw order, rollover policy, and reserved-vs-pool split MUST be carried in `pricingSnapshotRef`.
7. **Coupon overlay (Promotions)**: Apply eligible Coupon adjustments with `settlementCurrency = price` on the post-commitment line amount in price currency. Default stacking: `exclusive_best`. Record applied coupon id(s) and pre-/post-discount amounts.
8. **FX policy (PLAL abstraction)**: If invoice currency != price currency, PLAL MUST apply the FX table per policy (inputs from Finance); no implicit/provider-default FX without a policy record. Two policies: (a) per-window rate-lock (final at event time); (b) invoice-period FX (provisional amount on the hot path; re-rate by delta at period close — close-time `fxTableVersion` authoritative). Then apply coupons with `settlementCurrency = billing` to the billing-currency amount (same `fxTableVersion`).
9. **Emit monetary amounts (PLAL → Billing boundary)**: PLAL MUST emit amounts with precision sufficient for Billing; invoice rounding (per-line vs per-invoice) is applied by Billing, not PLAL. PLAL records the rounding policy id. The resolved per-line amount MUST NOT be negative before period-level phases.

> **Reserved-capacity component**: when `reservationMatch` is present, a reserved-capacity charge is evaluated at step 6 in one of two flavors: (a) consumption-flavor (matched usage at reserved rate, remainder on-demand); (b) capacity-flavor (`capacityCharge` on allocated `reservedQuantity` regardless of usage). The reserved portion is excluded from `commitmentPools[]`. Flavor and `reservedQuantity` frozen in `pricingSnapshotRef`.

> **Period-level phase (outside the per-line order)**: floor / cap per period are applied after step 9, over the period aggregate, by Billing (§17.2). Steps 1-9 are per-line and have no slot for period-level min/max.

#### PriceList scope mapping (used in step 4)

| **`PriceList.scope`** | **MUST match (evaluation context)** |
|-----------------------|-------------------------------------|
| `global` | Always eligible (subject to plan/SKU applicability) |
| `partner`, `orgTier` | `sellerTenantId` (channel/reseller that sold the subscription) |
| `brand` | Plan/SKU `brandId` at `t` |
| `region` | Usage or price-row `region` key |

Tenant axes NOT used as `PriceList.scope` filters: `resourceTenantId` (usage tenancy; MUST NOT alone match partner/orgTier rows); `payerTenantId` / `accountId` (contract/account overlays in step 5); `sellerTenantId` (used for `scope(partner|orgTier)`).

#### Determinism and Rating compatibility (preserved)

- **Pure function core**: determinism stated over the evaluation unit; for windowed models the window-aggregated `Q` for `(meter, dimensionKey, window)`. Given frozen inputs, the monetary outcome MUST be identical across replay, recompute, and cross-region batch workers.
- **Windowed `Q` ownership (single-writer)**: materialized and owned by the Rating `AggregationWindow`, single writer per partition key; concurrent re-resolve serializes on the partition key.
- **Non-negative resolved price**: MUST NOT go negative; clamp to zero or emit a structured credit (policy TBD).
- **Usage corrections / negative quantity**: deterministically reverse prior effect (refill pool, decrement `Q`), emit compensating deltas; never drive a line negative.
- **Snapshot carry / idempotency / delta idempotency / separation**: per §6.1.

#### Multi-currency (preserved)

- **Price currency**: currency of the `Price.amount` row selected in step 2; per-market list prices are first-class.
- **Presentment currency**: portal display FX, non-authoritative, outside PLAL; MUST be labelled estimates.
- **Billing currency**: invoice currency per payer account/contract; PLAL converts per step 8; per-window rate-lock final at event time; invoice-period FX emits provisional + re-rates by delta at close.
- **Coupons and currency**: price-currency coupons in step 7; billing-currency coupons after step 8.
- **PLAL / Finance boundary**: FX tables and lock policies owned by Finance; PLAL records `fxTableVersion` / locked-rate id.
- **PLAL / Billing boundary**: PLAL MUST NOT apply invoice rounding; Billing rounds in billing currency after conversion.

### 17.2 Boundary Contracts (coupons, floor/cap, plan-change proration)

**Coupons (Promotions boundary)** — normative order extends §17.1: Catalog base → Partner/OrgTier/brand/region (`PriceList`) → Customer (contract/account) → Commitment (step 6) → Coupon (step 7) → FX (step 8) → Emit (step 9). Coupons apply after customer overlay and after commitment math; default before FX (price currency), exception after FX for `settlementCurrency = billing`. Coupon + partner discount both apply (partner in step 4, coupon in step 7). Coupon + graduated tier: default on the total line amount after tier math; `applyPerTierBand = true` applies per marginal band. Stacking: `exclusive_best` (default — largest customer benefit, others excluded), `ordered_stack` (campaign-linked `stackSequence` only), incompatible pairs fail-closed at redemption bind. Consumption contract (Tariffs ← Promotions): a frozen coupon snapshot with at minimum `couponId`, `adjustmentType` (percent \| fixed_amount), `value`, `settlementCurrency` (price \| billing), `applyPerTierBand`, `applyScope` (`usage` \| `recurring` \| `line_total`, default `line_total`), `stackSequence` (required under `ordered_stack`), validity, applicability filters, redemption eligibility. Missing `applyScope` (or `stackSequence` under `ordered_stack`) MUST fail-closed.

**Period-level floor and cap** — period-level phases over the aggregated total; applied after step 9 by Billing. Attach to the usage component by default, or recurring+usage if plan-level (frozen in `pricingSnapshotRef`). Set in price currency, converted with the same FX policy/`fxTableVersion` as step 8 (billing-currency floor/cap compared after conversion; currency explicit, no implicit default). Tariffs sets the amount/currency/scope and emits `PeriodFloorCapObligation`; Billing executes `max(total, floor)` / `min(total, cap)`. The non-negative guard applies before floor/cap; a floor MUST NOT mask a negative line. Whether a contractual minimum-spend floor claws back coupon discount is unresolved (default proposal: floor compares post-coupon total) — §15.

**Plan-change proration** — Subscriptions owns WHEN and the up/down asymmetry policy (cross-PRD); Tariffs owns the evaluation semantics and consumes `(changeEffectiveAt, changeMode)`. On a plan change at `changeEffectiveAt`, rate planA over `[periodStart, changeEffectiveAt)` and planB over `[changeEffectiveAt, periodEnd)` (half-open, UTC), each against its own revision and snapshot, at full precision (Billing aggregates). Recurring component prorated on the configured `prorationBasis`. Tier `Q` carry-vs-reset and commitment-pool carry-vs-reset across the boundary frozen in the snapshot (default reset unless marked carry). Prorated corrections to an already-rated portion emitted as deltas via the Adjustment path. Tariffs consumes `changeMode` to pick the split point; the policy that sets the mode is Subscriptions.

### 17.3 Cloud Catalog Readiness and Phasing

The cloud-defining models for a genuine S3 + VM + Disks catalog that are in Scope: **Dimensional pricing** and **CAPACITY / reservation pricing** (consumption- and capacity-flavor), plus the **usage dimension-population contract**. **Composite meter is Follow-on** — no derived-meter primitive exists upstream yet, and VM is priced at launch via the instance-type dimension. The engine seams (`dimensionKey`, `reservationMatch` + `capacityCharge`, `commitmentPools[]`, `maxCumulativeMarkup`) admit the deferred models additively — no change to the published snapshot/Rating contract.

| **Item** | **Scope** | **Unlocks** | **Hard precondition (owner)** |
|----------|-----------|-------------|-------------------------------|
| Dimensional pricing | Scope | S3 by storage-class / region / operation; VM by instance type | OSS metering emission of dimension values (external; BSS declare/freeze + Rating pass-through owned here) — critical path |
| CAPACITY / reservation pricing | Scope | Provisioned Disks / IOPS, RI-style commitments | `reservationMatch` entitlement source (OSS / Contracts) |
| Composite meter | Follow-on | VM = vCPU + RAM as one priced line | Derived-meter primitive in Plan & Price / Catalog (must land first) |

**Sequencing**: (1) lock the BSS-side dimension contract (Tariffs declares + freezes; Rating passes `dimensionKey` through) and raise the OSS metering emission shape as an upstream requirement to the Usage Collector PRD — the external emission is the critical path and blocks Dimensional pricing. (2) Dimensional → (3) CAPACITY/reservation. Composite meter follows only after the derived-meter primitive is added to Plan & Price / Catalog. **Risk if the dimension contract slips**: `dimensionKey` stays the empty tuple and dimension combinations can only be expressed by minting a separate meter per combination — exploding catalog cardinality.

### 17.4 Future Scope

**Tariff semantics — formulas and computation**

| **Capability** | **Priority** | **Status** | **Notes** |
|----------------|--------------|------------|-----------|
| Percentage pricing (% of base amount) | `p2` | Follow-on | Marketplace/payments; new model row in Design |
| Bounded override composition (anti-drift caps) | `p2` | Follow-on | Contract defined now (`maxCumulativeMarkup` on the overlay chain, step 4); rich policy object phased |
| Minimum fee (floor) per period | `p2` | Follow-on | Boundary/contract defined now (§17.2); Tariffs sets amount, Billing executes; impl phased |
| Cap (ceiling) per period | `p2` | Follow-on | Boundary/contract defined now (§17.2); bill-shock protection executed by Billing post-aggregation; impl phased |
| Two-dimensional pricing (seats x usage) | `p2` | Follow-on | Multiple meters + hybrid model; Subscriptions seat count input |
| Composite meter (formula across meters; VM = vCPU + RAM as one line) | `p2` | Follow-on | Blocked on a derived-meter primitive in Plan & Price / Catalog; at launch VM priced via instance-type as `dimensionKey` |
| Non-negative price after stacked discounts | `p3` | Deferred | Guard is normative (§6.1); only the clamp-vs-credit policy is deferred to Finance workshop |

**Plan structure and effective dating**

| **Capability** | **Priority** | **Status** | **Notes** |
|----------------|--------------|------------|-----------|
| Extended multi-SLA tier packs (beyond manifest `PlanTier`) | `p2` | Follow-on | Current Scope uses `PlanTier` only; full tier bundles with per-tier SLA packs |
| Plan change policy (immediate vs end-of-term, asymmetric up/down) | `p2` | Cross-PRD | WHEN/asymmetry owned by Subscriptions; Tariffs proration semantics defined now (§17.2, AC 17) |

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
| BYOL / license-attached discount | `p2` | Cross-PRD | Entitlement in OSS/Contracts; Tariffs consumes license flag in ctx |
| Retroactive volume tier on monthly accumulation | `p2` | Follow-on | Batch re-rate at period close; open-window late-arrival semantics defined now (AC 10) |
| Burstable credits, storage tier transitions, spot pricing | `p3` | Deferred | Cloud provider advanced |

---

*Child artifacts: ADR(s) for precedence conflicts and snapshot versioning strategy; DESIGN for Tariffs / PLAL ↔ Rating / Finance FX integration contracts and evaluation traces.*

