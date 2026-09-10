---
refs:
  - bss/manifest/vz-arch-manifest-bss-only.md
  - bss/prd/PRD-billing-ledger-balances-202604041200
  - bss/prd/PRD-contracts-agreements-202601120119
  - bss/prd/PRD-metering-pricing-module-202601120119
  - bss/prd/PRD-product-catalog-marketplace-202601120119
  - bss/prd/PRD-product-catalog-marketplace-202601120119/UC-effective-dating-price-windows-202601121200.md
  - bss/prd/PRD-product-catalog-marketplace-202601120119/UC-plan-price-modeling-202601121200.md
  - bss/prd/PRD-product-sku-management-202606101924
  - bss/prd/PRD-rating-engine-202604031200
  - bss/prd/PRD-subscriptions-entitlements-202601120119
  - bss/prd/PRD-tariffs-pricing-logic-202604011200
---

Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

# PRD — Plan & Price Modeling

> **2026-09-10 amendment:** plan list/detail expose the same `pending_approvals`
> array; taxonomy lists/details expose reference counts and pending proposals
> with approval-read-gated content; approving a taxonomy edit applies
> it atomically. Tax-category definitions come from Product Catalog, while the
> agreed assignment target is only `price.tax_category_ref`, not SKU/region
> defaults. The dictionary is implemented; region migration and membership
> validation remain pending. See [current contracts and examples](./design/ui-read-contracts.md).
> Approval GETs now add current participant names via AM/IdP, without storing
> them in Pricing. Missing permissions/source/profile are explicit states;
> UUID batches are implemented; live IAM and service-principal integration remain
> open (D-365). Enriched plan/taxonomy GETs always return fresh no-store bodies,
> without changing write preconditions (D-366).
> D-367 adds filters/sorts on the shown plan, stable original creation time and
> approval state counts under the original read scope. The shared OData toolkit
> is not extended; clients use scalar `model_kind`/`currency` query predicates.

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Architecture Alignment](#2-architecture-alignment)
  - [2.1 Boundaries](#21-boundaries)
  - [2.2 Canonical Scope Key (normative)](#22-canonical-scope-key-normative)
  - [2.3 Predecessor Documents and Scope Migration](#23-predecessor-documents-and-scope-migration)
- [3. Actors](#3-actors)
  - [3.1 Human Actors](#31-human-actors)
  - [3.2 System Actors](#32-system-actors)
- [4. Operational Concept & Environment](#4-operational-concept--environment)
  - [4.1 Module-Specific Environment Constraints](#41-module-specific-environment-constraints)
- [5. Scope](#5-scope)
  - [5.1 In Scope](#51-in-scope)
  - [5.2 Out of Scope](#52-out-of-scope)
- [6. Functional Requirements](#6-functional-requirements)
  - [6.1 Plan Definition and Billing Cycles](#61-plan-definition-and-billing-cycles)
  - [6.2 Price Structure and Model Kinds](#62-price-structure-and-model-kinds)
  - [6.3 Plan Composition, Descriptors, and Phases](#63-plan-composition-descriptors-and-phases)
  - [6.4 Multi-Currency, Regions, and Tax Display](#64-multi-currency-regions-and-tax-display)
  - [6.5 PriceWindow Linkage and Coverage](#65-pricewindow-linkage-and-coverage)
  - [6.6 Price Overlays and Segment (Customer-Group) Pricing](#66-price-overlays-and-segment-customer-group-pricing)
  - [6.7 Publish Validation, Approval, and Events](#67-publish-validation-approval-and-events)
  - [6.8 Plan Lifecycle: Versioning, Retirement, Migration](#68-plan-lifecycle-versioning-retirement-migration)
  - [6.9 Consumer Contracts (Subscriptions / Rating / Billing)](#69-consumer-contracts-subscriptions--rating--billing)
  - [6.10 Advanced Pricing Primitives](#610-advanced-pricing-primitives)
  - [6.11 Operator Efficiency](#611-operator-efficiency)
  - [6.12 Access Control and Governance](#612-access-control-and-governance)
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
  - [Plan definition and billing cycles](#plan-definition-and-billing-cycles)
  - [Plan composition, phases, and descriptors](#plan-composition-phases-and-descriptors)
  - [Pricing tiers and usage policy](#pricing-tiers-and-usage-policy)
  - [Multi-currency, tax display, and price rows](#multi-currency-tax-display-and-price-rows)
  - [Add-ons and bundles](#add-ons-and-bundles)
  - [PriceWindow linkage](#pricewindow-linkage)
  - [Publish validation, approval, and events](#publish-validation-approval-and-events)
  - [Plan lifecycle, update, and migration](#plan-lifecycle-update-and-migration)
  - [Operator efficiency](#operator-efficiency)
  - [Price preview and access control](#price-preview-and-access-control)
  - [Commercial model coverage](#commercial-model-coverage)
  - [Proration and provisioning contracts](#proration-and-provisioning-contracts)
  - [Currency and FX policy](#currency-and-fx-policy)
  - [Lifecycle safety (contract lock and entitlement overflow)](#lifecycle-safety-contract-lock-and-entitlement-overflow)
  - [Read-model and publish robustness](#read-model-and-publish-robustness)
  - [Phases and PriceOverlay authoring](#phases-and-priceoverlay-authoring)
  - [Migration robustness and eligibility](#migration-robustness-and-eligibility)
  - [Cross-PRD contract conformance](#cross-prd-contract-conformance)
  - [Approval, governance, and access control](#approval-governance-and-access-control)
  - [Discounts and approval governance](#discounts-and-approval-governance)
  - [Validation completeness](#validation-completeness)
  - [Grandfathering and concurrency](#grandfathering-and-concurrency)
  - [Scope-key and tax validation](#scope-key-and-tax-validation)
  - [Lifecycle and structural integrity](#lifecycle-and-structural-integrity)
  - [Currency binding and lifecycle integrity](#currency-binding-and-lifecycle-integrity)
  - [Authoring guardrails and discount GA gate](#authoring-guardrails-and-discount-ga-gate)
  - [Package and prepaid pricing](#package-and-prepaid-pricing)
  - [Non-Functional Requirements (Show-Stoppers)](#non-functional-requirements-show-stoppers)
  - [Commercial-shape and lifecycle completeness (review follow-ups)](#commercial-shape-and-lifecycle-completeness-review-follow-ups)
  - [Governance and referential-integrity completeness (review follow-ups)](#governance-and-referential-integrity-completeness-review-follow-ups)
  - [Instant precision and grandfathering-field placement (implementation-finding follow-ups)](#instant-precision-and-grandfathering-field-placement-implementation-finding-follow-ups)
- [13. Dependencies](#13-dependencies)
- [14. Assumptions](#14-assumptions)
- [15. Open Questions](#15-open-questions)
- [16. Risks](#16-risks)
- [17. Reference Materials](#17-reference-materials)
  - [17.1 Supported Billing Cycles and Price Structure Kinds (catalog)](#171-supported-billing-cycles-and-price-structure-kinds-catalog)
  - [17.2 Model Kind / Tariffs Formula Mapping (conformance)](#172-model-kind--tariffs-formula-mapping-conformance)
  - [17.3 Plan Composition Rules (normative)](#173-plan-composition-rules-normative)
  - [17.4 Price Validation Rules (catalog)](#174-price-validation-rules-catalog)
  - [17.5 Price-Change Mechanisms and CatalogVersion Increment Contract](#175-price-change-mechanisms-and-catalogversion-increment-contract)
  - [17.6 Consumer Contracts Detail](#176-consumer-contracts-detail)
  - [17.7 Advanced Pricing Primitives Detail](#177-advanced-pricing-primitives-detail)
  - [17.8 Future Scope](#178-future-scope)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

**Plan & Price Modeling** is the BSS Product Catalog capability that provides the authoritative way to define **subscription plans**, **price structures**, **add-ons**, **bundles**, and **billing descriptors** so that **Subscriptions** can sell, **Tariffs** can resolve commercial inputs, and **Rating** can charge **deterministically** and **reproducibly** from frozen snapshots.

The Catalog remains **System of Record** for `Plan`, `Price`, bundle/add-on composition rules, and billing descriptors. This capability defines **what** those primitives MUST contain so **Tariffs** can evaluate and **Rating** can charge deterministically; it does **not** compute charges, evaluate overlays, or manage subscription runtime state (§5.2).

> Where this PRD states a normative requirement, the **subject is the catalog (this PRD)** unless explicitly stated otherwise.

### 1.2 Background / Problem Statement

The Catalog is the SoR for the pricing primitives that downstream commercial systems consume. Without an explicit, publish-validated structure for plans and prices, rating inputs become ambiguous: overlapping tier bands, missing evaluation-policy fields, undeclared meters, and uncovered price windows reach production and produce non-reproducible charges, revenue leakage, and disputes.

This PRD fixes the **catalog-side structure and publish contract** — billing cycles, tier bands, model kinds, `PlanTier`, evaluation-policy fields, billing descriptors, price windows, and the consumer read-model contract — so that a published plan exposes a complete, frozen read model for Tariffs and Rating and so that no ambiguous configuration can be published (fail-closed validation). It complements **Tariffs** (evaluation semantics), **Subscriptions** (runtime, proration math), and **Billing** (posting, invoice immutability).

Industry practice for plan migrations favors **time-bounded grandfathering**, renewal-aligned cutovers, and advance notice (typically 60-90+ days for material increases). The catalog MUST support `priceEligibility` and scheduled migration paths so operators can implement these policies without cloning SKUs.

### 1.3 Goals (Business Outcomes)

- **Revenue accuracy**: zero ambiguous plan/price configuration reaching production; fail-closed publish validation.
- **Time-to-market**: self-service plan wizard, clone, and bulk price maintenance for Finance and Product teams (target >= 90% self-service for standard plan types).
- **Partner and marketplace enablement**: multi-currency, regional, and rev-share-aware bundles without SKU cloning.
- **Lifecycle safety**: retirement, grandfathering, and scheduled migration without mutating posted invoices.
- **Audit and compliance**: immutable price history, approval trails, and snapshot refs with **>= 7-year retention** (tenant/jurisdiction-configurable).
- **Engineering determinism**: published plans expose a complete read model (`{skuId, planId, priceId}`, model kind, tier bands, evaluation-policy fields, billing descriptors); plan mutations are versioned; active subscriptions continue on frozen snapshots until renewal or explicit migration; event fan-out enables cache warming within manifest SLOs.

**Success metrics**

| **Metric** | **Target** | **Measurement** |
|------------|------------|-----------------|
| Publish validation catch rate | 100% of known-invalid configs blocked before `PlanPublished` | QA + production validation audit |
| Pricing config incidents | Zero production charges from ambiguous meter or overlapping tier config | Incident tickets tagged `catalog-pricing` |
| Pre-GA config-validation coverage (leading indicator) | >= 99% of seeded-invalid configs caught in staging before GA | Staging validation-suite pass rate |
| Plan publish propagation | p95 <= 5s from `CatalogVersionPublished` (committed version) to Rating read-model visibility | Event timestamp delta |
| Plan read / preview latency | p95 < 100ms per tenant partition | APM on catalog read APIs |
| Manual IT involvement in plan creation | >= 90% self-service for standard plan types | Ops ticket volume vs plan publish count |
| Price history completeness | 100% of published price changes retain prior row | Audit sampling |
| Migration without invoice mutation | 100% of scheduled migrations use snapshot/`PlanLink` paths only | Billing reconciliation |

### 1.4 Glossary

| **Term** | **Definition** |
|----------|----------------|
| **Plan** | Catalog entity binding a published SKU to a **billing cycle** (one-time, recurring, usage-based, hybrid), add-on rules, optional `invoiceGroupingKey`, mandatory **PlanTier**, and optional phase price schedules. |
| **Plan composition** | Commercial elements on a plan: base and usage price row(s), tier bands, add-on rules, billing descriptors — not runtime subscription state. |
| **Price row** | Catalog `Price`: amount, currency, the **canonical scope key** `(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)` (**no `brand` axis** — brand pricing is a brand-scoped `PriceOverlay`), `minQtyThreshold`, rounding policy reference, `taxInclusive`, `billingTiming` (**required** on recurring rows), tier bands, the **evaluation-policy fields** (the nine-field roster normative in [`design/01-foundation.md`](./design/01-foundation.md) §4.4 — this entry named three of them until **D-162**, 2026-08-03, wrote the set down, D-40, D-44 and D-45 having each added to it in the meantime), and an optional `discountRef`. `grandfatherUntil` is optional but **confined**: it may be non-null only on a row whose `priceEligibility = existing_grandfathered` (**D-147**, 2026-08-02 implementation finding — listing it here as a plain optional field admitted the reading that any row may carry a sell-until bound). |
| **Pricing tier** | Quantity band `[fromQty, toQty)` with a unit price (graduated/volume); bands MUST be non-overlapping and ordered ascending by `fromQty`. Whole-block pricing is a separate `modelKind=package`, not a tier band. An **included allowance** (N free units bundled in a recurring fee) is authored **first-class** on the usage row (`includedAllowance`, D-45): `rollover = none` **compiles at publish** to the `$0` first band `[0, N)` plus a frozen first-class marker (display "includes N units"; reporting separates included from billed); `rollover = carry` compiles to the prepaid-grant machinery (D-43). See `fr-included-allowance` for constraints. |
| **Included allowance** | First-class `includedAllowance = { quantity N > 0 (in the row's billable units), rolloverPolicy ∈ {none (default), carry(maxPeriods ≥ 1)} }` on a `usage` price row (D-45, supersedes the F-32 $0-band-only stance). Publish-**compiled**: `none` → the `$0` first band + frozen allowance marker (evaluation = existing band math, no rating change); `carry` → a per-period **promotional grant** scoped to the row's meter (D-43 machinery; Billing executes drawdown; the catalog holds no balance). Launch constraints: `sum` rows only (level-meter allowance is Future — the boundary in level·granule units varies with period length), no per-seat scaling (Future decision gate), and a row MUST NOT combine an authored `$0` first band with `includedAllowance` (double-free; publish-blocked). |
| **Billing granularity** | Minimum billable unit for usage (`per_second`, `per_minute`, `per_hour`, `per_day`, whole-unit). MUST be set on usage price rows at publish. Tier-band quantities (`fromQty`/`toQty`) are expressed in **billable units after this quantization** (e.g. `per_hour` bands count hours, never raw seconds). |
| **Derived (composite) meter** | A catalog primitive binding a **formula over two or more published metering units** (e.g. `vCPU`, `RAM`) to a **single derived metering unit** priced as one line. The catalog persists the **constituent unit ids**, the **formula expression (as data)**, and the **output unit**; the constituent base units — and the **derived output unit itself** — are declared by the catalog registry (one meter namespace; this PRD binds the formula, it declares no meters). Tariffs evaluates the formula at rating time (math is **not** computed here); the composite definition is frozen in `pricingSnapshotRef`. |
| **Trailing-tier qualification** | Optional tiered-usage-row primitive `tierQualificationWindow` (`current` \| `trailing_period`), distinct from `tierAggregationWindow` and `billingGranularity`. `trailing_period` qualifies the **rate tier from the prior period's total** (single-band selection), **locks that rate** for the current period, and bills actual usage at `billingGranularity` (the period is the subscription's **anchor-derived billing period**, not a calendar month; canonical: PaaS egress — the prior period's volume sets `$/GiB`, billed hourly). The lock is **per subscription per billing period** and therefore does **not** live in `pricingSnapshotRef` — that reference is plan-scoped, stamped at publish and immutable, while one published row serves N subscriptions with N anchors and N trailing totals: the catalog freezes only the authored window and band set, and **Rating** pins `{priceId, periodStart, qualifiedBandId, lockedUnitPriceMinor}` on the period's rated charges beside the snapshot — that pin is the replay source (D-60; propagation completed by D-69). Catalog authors the window; Rating supplies the trailing aggregate and re-qualifies; Tariffs applies the locked rate. |
| **Hybrid plan** | Plan combining **recurring base** price row(s) and **usage** price row(s) on the same `planId` for one SKU. |
| **Plan phase** | Ordered segment (kind `trial`, `interim` or `evergreen` — `interim` was spelled `intro` until **D-358**, 2026-09-09) with an optional operator label `displayName` (**D-357**: free-form, no behaviour, not an identity; absent, a UI derives a label from the kind and position), its own price schedule reference and optional `convertsToPhaseId` successor. Catalog publishes the phase->price mapping, ordering, and each **non-terminal** phase's authored **`phaseDurationDays`** (> 0; REQUIRED at publish — `convertsToPhaseId` says *where* a phase converts, the duration says *when*; a terminal phase MUST NOT carry one). `displayTrialDays` is the `trial` phase's published `phaseDurationDays` (one value, PRD-named projection for preview/quoting) — authorable **only** on a `trial` phase, and only on one that carries the duration it projects (`DISPLAY_TRIAL_DAYS_INVALID` otherwise, **D-151**). **Phase runtime mechanics** are owned by Subscriptions, which enforces from these published durations. Every phase MUST be covered by recurring rows per sold `(currency, region)`; **usage rows are phase-invariant by default** (an explicit phase-scoped usage row wins for its phase). |
| **Add-on rule** | Plan-scoped rule: eligible add-on SKU, required/optional, min/max/step quantity, optional price override reference. |
| **Bundle** | Composite of included SKUs with constraints, optional `revShare`, and `invoiceItemization` (`aggregate` \| `itemize`). For `sum_of_parts` pricing the bundle references the component **`planId`s** (not bare SKUs) so the summed rows are unambiguous per `(currency, region)` (and matching `frequency` for recurring components). |
| **PlanTier** | Manifest-required classification on SKU/Plan for Subscriptions, SLA, and quotas; distinct from **OrgTier** partner overlays. |
| **Billing descriptor set** | Per published SKU/Plan: invoice line template/labels, GL code, composition/itemization rules (manifest §4.1 contract); the v1 contract's other two elements — `taxCategory` and `billingTiming` — ride the **price row** rather than this set (D-48 + D-110), since a tax category and a billing timing are per row. Plan-owned customer-facing content (plan/phase names, invoice line labels) is **single-language today**; per-locale **authoring** is owned by extending the registry's localization mechanism to Plan-owned fields, while **Presentation** owns invoice/preview rendering localization (decision — Open Questions). |
| **priceEligibility** | Who may receive a price/window: `all_subscriptions`, `new_subscriptions_only`, `existing_grandfathered`. When more than one class holds an active window on the same remaining axes, selection is **most-specific-wins**: `existing_grandfathered` > `new_subscriptions_only` > `all_subscriptions` (Tariffs step 2 applies this **after** eligibility matching). Within `existing_grandfathered`, the concrete **generation** is selected by the subscription's bound `cohort` — the cohort of its pinned price id (`pricingSnapshotRef`) — so the resolved row is always unique. The **successor** row in a grandfathering cutover carries `all_subscriptions`, so new subscriptions bind to it and a grandfathered subscription re-bound at `grandfatherUntil` expiry also resolves to it. |
| **pricingSnapshotRef** | Immutable composite reference (`CatalogVersion` + resolved price ids + evaluation-policy version) on charges and `BillableItem`. The **evaluation-policy version** is the **evaluation-policy generation** — a declared constant of this gear, format `ep-<n>`, naming which evaluation-policy field set the frozen row content is read under; roster, bump rule and log in [`design/01-foundation.md`](./design/01-foundation.md) §4.4 (**D-162**). **Normative composition SoR**: Tariffs; this entry is the aligned catalog-side view and MUST NOT diverge from it. |
| **Plan migration** | Transition from retiring `planId` to target plan via Subscriptions `PlanLink` — not in-place posted invoice mutation. |
| **Grandfathering** | Carrying legacy pricing forward on a dedicated `existing_grandfathered` row for subscriptions activated before cutover (the row is immutable, so the retained price is necessarily a grandfathered **copy**); optionally **time-bound** via `grandfatherUntil` (null = indefinite). Grandfathering is **multi-generation**: each cutover creates a new `cohort` generation, and prior generations stay live and untouched. Because `priceEligibility` and `cohort` are part of the canonical scope key, every generation and the successor are **distinct keys** that hold active windows concurrently at the same `t` (no non-overlap violation); grandfathered subscriptions are **live-resolved** by Tariffs step 2 against an **immutable** row, so the price is stable, reconciling live resolution with the frozen-snapshot doctrine. |
| **Plan retirement** | Blocks **new** subscriptions; preserves snapshots for in-flight subscribers. |
| **Entitlement grant set** | The feature flags and quotas a Plan confers, published into the catalog read model so Subscriptions can provision and enforce them. Policy is `PlanTier`-driven (registry); enforcement is owned by Subscriptions. This PRD owns **publishing the grant set (or its `PlanTier`-resolved reference)** on the Plan, not entitlement semantics. **MAY be phase-scoped** (`phase→grant-set map`, D-41) so a trial phase confers tighter caps than evergreen; Subscriptions resolves it for the active phase at `t`. |
| **Proration input contract** | Read-model fields each recurring price row exposes so the downstream plan-change path is deterministic: `billingAnchorPolicy`, `prorationBasis`, and `creditOnDowngrade` (bool). Catalog publishes the inputs; Subscriptions owns the change boundary/mode, the rating gear the proration math (rating §6.11/§17.2). |
| **prorationBasis** | **Canonical** apportionment convention for a partial period — **one enum owned here and adopted verbatim by Tariffs**: `calendar_days_actual`, `calendar_days_30` (fixed 30-day month, day count capped at 30), `by_second`, `whole_unit` (no sub-period proration), or `none` (no proration — full-period charge, no partial credit). MUST be authored on the plan/price policy and **frozen in `pricingSnapshotRef`**; **all** mid-period proration uses it. Catalog names the convention; Subscriptions and Tariffs compute the amount from the same frozen value. |
| **billingAnchorPolicy** | When a recurring period anchors: `calendar_month` (1st), `subscription_start` (signup day), or `fixed_day(d)`. For `fixed_day(d)` — and for a `subscription_start` anchor under monthly-granular cycles incl. `customEveryN Months(n)` — a day exceeding the month length MUST clamp to the **last day of the month**, with the **anchor day preserved** across periods (each period clamps independently: Jan 31 → Feb 28 → Mar 31, no drift); all anchor math is **UTC**. |
| **billingTiming** | When a recurring fee is invoiced relative to its service period: `in_advance` (billed at period start) or `in_arrears` (billed at period end). **REQUIRED** on recurring price rows at publish; frozen in `pricingSnapshotRef`. Usage rows are implicitly `in_arrears`. Consumed by **Billing** (deferral policy) and Subscriptions; a hybrid plan MAY combine an `in_advance` base row with `in_arrears` usage rows. |
| **chargeKind** | Price-row axis distinguishing the charge components a plan carries at once: `recurring`, `usage`, `one_time` (a **one-time plan's base row**), or `one_time_setup` (a setup charge on a recurring/hybrid plan). Part of the canonical scope key, so a hybrid plan's components are **distinct** rows, not duplicates. |
| **one_time_setup** | A `chargeKind` for an optional one-time setup/activation charge on a recurring/hybrid plan; a first-class price row (not an add-on SKU). Charged **once per subscription lifetime**: at activation, or for a plan with a `trial` phase at entry into the **first non-trial phase** (a cancelled trial is never charged); a plan change or `PlanLink` migration **MUST NOT** charge the target plan's setup row at all — whether or not the origin plan carried one (setup is tied to subscription activation, not plan entry). |
| **reservedRate / reservationFlavor** | Attributes on a `usage` price row for reserved-capacity pricing: `reservedRate` is the committed unit price for the reserved/allocated quantity; `reservationFlavor` is `consumption` (matched usage at the reserved rate, remainder on-demand) or `capacity` (allocated quantity billed regardless of usage). Aligned with Tariffs `reservationMatch`. |
| **allowedChangeTargets** | Plan-change-contract field: the target `planId`s a subscription MAY move to (**explicit list**; rule-based targets are Future scope); each edge's boundary classification (`in_place` \| `cancel_plus_new`) is **computed at change time by Subscriptions** from both plans' published market/frequency facts at its pinned version (D-93 — a publish-time stamp cannot be re-computed under the frozen read model); absence = **no self-service change** (fail-safe). Subscriptions enforces. |
| **comparabilityRank** | Plan-change-contract integer rank classifying a change as upgrade (higher), downgrade (lower), or switch (equal); drives proration sign/credit. `PlanTier` is **never** an ordering at launch — `comparabilityRank` is REQUIRED for every plan in self-service change; an authoritative published `PlanTier` ordering is §17.8 Future (S6 K4; D-68). |
| **availableFrom / availableTo** | Optional plan-level **purchasability dates** for **all billing cycles** (unified one pair). Validated against window coverage at publish; a purchase still requires an active window (sellability gate). |
| **PLAL** | Pricing Logic Abstraction Layer — the Tariffs evaluation surface resolving overlays, commitment, coupons, and FX. Catalog produces inputs; PLAL evaluates. |
| **SoR** | System of Record — authoritative owner of an entity's truth (e.g. Catalog registry is SoR for Product/SKU/`CatalogVersion`). |
| **SSP** | Standalone Selling Price — ASC 606 allocation reference; out of scope here (catalog MAY carry SSP tags in Future scope). |
| **OrgTier** | A partner's commercial standing/overlay axis (channel economics); distinct from `PlanTier` (the edition/grade of a SKU). OrgTier-scoped pricing is evaluated in Tariffs. |
| **invoiceGroupingKey** | Optional Plan field that groups billable lines onto the same invoice/section; consumed by Billing for line layout. Optional (empty = no grouping). It is a **layout hint only** and MUST NOT override the single-currency-per-invoice invariant: Billing MUST split lines of different currencies onto separate invoices even when their grouping keys match. |
| **Rounding policy reference** | A named rounding-policy id referenced by a price row; policy definition and application are owned downstream (Tariffs/Billing). Catalog persists the reference only. If unset on a row, publish MUST resolve the **tenant default rounding policy**; if neither exists, publish MUST fail (no implicit rounding). |
| **dimensionKey** | Identifier for a usage dimension on a `(meter, dimensionKey)` rated line; dimensional pricing by event properties is Future scope. |
| **region** | Catalog price-scope key (a **commercial/billing territory**, e.g. `US`, `EU`), part of the canonical scope key. It is **not** a tax jurisdiction (Tax Engine maps region -> tax jurisdiction) and is **independent of the authorization region** in IdP claims: a user's authz scope governs *which* rows they MAY mutate, while pricing `region` is the row's commercial scope. Region taxonomy is tenant-configured. |
| **brand** | A tenant sub-brand/label. **Not** a price-row axis (the manifest `Price` model has no `brand` field): brand-differentiated pricing is authored as a **brand-scoped `PriceOverlay`** overlay (`PriceOverlay.scope=brand`; Tariffs evaluates the overlay). A brand-scoped `PriceOverlay`'s `brand` MUST be a member of the tenant's configured **brand taxonomy** (validated at save); tenant-configured like `region`. `brand` MAY additionally be an authorization/display scope. |
| **customerGroup** | Operator-defined end-customer segment (trial/beta/VIP/on-discount/custom) for **segment pricing** via a `customerGroup`-scoped `PriceOverlay` (a **set of adjustment lines** keyed `(planId?, targetSku?)` — D-42; per line percent, or per-currency amounts under D-08 — resolved by `payerTenantId`, most-specific line wins; lines carry **no region axis**: region variance comes from the region-scoped base rows a line applies to). A **BSS-owned governed taxonomy**; membership is an **effective-dated, audited BSS record** on the payer's commercial profile (not an AMS/tenant-topology attribute). The resolved group is frozen in `pricingSnapshotRef`. |
| **Plan phase types** | `trial` = time-boxed evaluation phase (runtime owned by Subscriptions); `interim` = **any** time-boxed non-trial phase between entry and the terminal phase — an introductory price, an onboarding period, a ramp step; its business meaning is the operator's `displayName`, not the kind (**D-357**, **D-358**; the kind was spelled `intro` until 2026-09-09, and frozen snapshots from before keep that spelling); `evergreen` = the **terminal** steady-state phase with no `convertsToPhaseId` successor. A phased plan MUST have exactly one terminal phase. |
| **Material change** | A plan/price change whose delta is **>= the configured approval threshold** (absolute or percent, per currency; for multi-currency changes, any row exceeding its own-currency threshold). If **no** threshold is configured, **all** changes are material (fail-safe). A **first publish** has **no prior baseline**, so a delta cannot be computed — it is **always material** (fail-safe). **Two-person rule** = the **submitter plus one independent approver** (two distinct principals); the submitter MUST NOT be the approver. A change **below** an explicitly configured threshold (and not a first publish) MAY auto-publish with no additional approver. |
| **grandfatherUntil** | Optional **UTC** bound on a grandfathered price row (per generation — each `cohort` carries its own). While null, grandfathering is **indefinite** across renewals; once passed, a bound subscription MUST re-bind to the current eligible row at its next renewal (catalog publishes the bound, Subscriptions executes). Non-null **only** on a row whose `priceEligibility = existing_grandfathered` — the pairing is one-directional and publish-enforced (**D-147**, 2026-08-02 implementation finding: it was a physical column check standing behind no stated rule, so its violation reached the operator as an internal fault rather than as a refusal they could act on). It is **not** a general per-row availability bound: `PriceWindow` `effectiveTo` and the plan's `availableTo` already carry "this row stops being sellable at `T`", and only the eligibility machinery derives a signal from this field. |
| **cohort** | Scope-key axis: the grandfathering **generation** discriminator — the UTC cutover instant that created the generation, at **millisecond resolution** (**D-144**, 2026-08-02 implementation finding: the axis is matched for **equality**, and Rating's D-126 bootstrap compares it against a window `effectiveTo` produced by a different code path in a different gear — two renderings of one moment at different resolutions are not equal, so an unquantized axis makes a generation unfindable by exactly the subscribers grandfathering exists to protect). `none` on every non-grandfathered row (publish-enforced: `cohort ≠ none` ⇔ `priceEligibility = existing_grandfathered`). Selection among generations is by the subscription's pinned price id, never by class specificity. Unrelated to `customerGroup` segment pricing (rationale: Design ADR set). |
| **minQtyThreshold** | A floor on a price row; **distinct** from tier `fromQty`. Its semantics depend on the billing cycle and MUST be one of two **explicitly-typed** floors: **(a) purchase-quantity floor** — enforced by **Subscriptions at order time**: a purchase below the floor is **rejected** (not silently zero); **(b) usage-quantity floor** — an **eligibility** floor evaluated **downstream** (Tariffs/Rating): usage below the floor is **not eligible** for the row's pricing and MUST **fail closed** to the row's declared fallback, **not** be silently zero-rated. The row MUST declare which floor type applies; a `usage` floor additionally declares its fallback (launch: `exception` only — the rating exception path). Catalog persists the type + value + fallback and freezes them in `pricingSnapshotRef`. Both MAY be set. |
| **discountRef** | Optional reference to an **external** discount instrument (owned by Promotions/Tariffs). The catalog validates **referential integrity only** — it does not author, evaluate, or stack discounts. Conditional day-1 hook. |

## 2. Architecture Alignment

| **Field** | **Value** |
|-----------|----------|
| **Applicable Manifest(s)** | BSS |
| **Relevant Chapters** | §4.1 Product and Service Catalog (Plan/Price, bundles, add-ons, billing descriptors, approvals, publish); §4.2 Rating and Charging (`pricingSnapshotRef`, stable catalog refs); §4.3 Subscriptions and Entitlements (`PlanLink`, composition); §2.1.3 Multi-tenant semantics (`PlanTier`, `{resourceTenantId, payerTenantId, sellerTenantId}`); §4.8 Marketplace (bundle rev-share where applicable) |

> **Normative alignment**: Catalog remains **SoR** for `Plan`, `Price`, bundle/add-on composition rules, and billing descriptors. This PRD defines **what** those primitives MUST contain so **Tariffs** can evaluate and **Rating** can charge deterministically. It MUST NOT contradict manifest invariants: non-overlapping `PriceWindow` rows per the **canonical scope key** (§2.2), immutable posted financials, stable `{skuId, planId, priceId}` on billable artifacts, and `pricingSnapshotRef` on outputs. **Approval interpretation**: the manifest's multi-approver workflow is read as multi-**party** — a material change requires the submitter **plus >= 1 independent approver** (two-person rule; see Glossary Material change).

### 2.1 Boundaries

> **Boundary with Tariffs**: This PRD owns catalog-side **structure and publish contract** (billing cycles, tier bands, add-on rules, `PlanTier`, model kind, evaluation-policy fields). **Tariffs** owns **evaluation semantics** (graduated vs volume math, override hierarchy, coupons, FX steps). **Rating** owns Usage -> `RatedCharge` / `BillableItem` orchestration.

> **Boundary with Price Windows**: **consolidated** (§15, answered 2026-07-10) — window scheduling, the UTC activation job, and `PriceWindowScheduled`/`Activated`/`Cancelled`/`Expired` event emission are **owned by this PRD's design** (the pricing gear; scheduler UI in the Frontend DESIGN). The legacy effective-dating use case is retained as scenario source material. Every billable `Price` row MUST be **linkable** to effective windows and **publish-validated** so Tariffs step 2 never resolves from draft-only state.

> **Boundary with Catalog registry (Product & SKU)**: `Product`, `SKU` (including the `bundle` **type** flag), `Category`, `Attribute`, the `PlanTier` **taxonomy**, the metering-unit **declaration**, and the catalog-wide **`CatalogVersion`** are owned by the Product & SKU registry. This PRD **consumes published SKUs** and **freezes** its plan/price/descriptor content into the `CatalogVersion` that the registry publishes; it MUST NOT re-author Product/SKU/Category/Attribute or re-define `CatalogVersion`. This PRD owns **Plan**, **Price**, **PriceWindow** linkage, **PriceOverlay authoring**, **bundle composition**, **add-on rules**, and **billing descriptors**.

### 2.2 Canonical Scope Key (normative)

The single scope key for row-uniqueness, supersession, `PriceWindow` non-overlap, and window coverage is **`(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)`**. `chargeKind` in `recurring | usage | one_time | one_time_setup` distinguishes the price components a single plan legitimately carries at once; `cohort` (default `none`) is the grandfathering **generation** discriminator — set to the cutover instant on `existing_grandfathered` rows only, so repeated cutovers create coexisting generations — a hybrid plan holds a `recurring` **and** a `usage` row (optionally a `one_time_setup` row) on one `planId`, and a **one-time plan's base row is `one_time`**, so they are **distinct keys**, not duplicates.

Axis values / defaults: `priceOverlay = base`; `phase =` the plan's **terminal `phase_id`** — the axis is always id-typed: a phased plan uses its authored terminal phase, a non-phased/one-time plan gets an implicit terminal phase (kind `evergreen`) auto-created at plan creation (setup rows ride the same id); `priceEligibility = all_subscriptions`; `chargeKind` per row; `cohort = none`. **Price rows authored here always carry `priceOverlay = base`** — partner/orgTier/brand overlays are separate `PriceOverlay` rows evaluated downstream by Tariffs; publish-time coverage resolves on the **base** key (overlays applied at evaluation). **`brand` is NOT a price-row axis**: brand-differentiated pricing is a **brand-scoped `PriceOverlay`** overlay, consistent with the manifest §4.1 invariant. The manifest §4.1 and §8.2 non-overlap invariants and the Tariffs non-overlap key are aligned to this key (extended additively with `phase`, `priceEligibility`, `chargeKind`); the effective-dating use case's narrower `(plan, currency, region, priceOverlay)` key is **superseded** by this canonical key for normative purposes.

### 2.3 Predecessor Documents and Scope Migration

| **Source** | **Relationship** |
|------------|------------------|
| Product Catalog & Marketplace PRD | Parent catalog PRD; Plan & Price Modeling was P0 scope with a use-case child |
| Product & SKU Management PRD | Catalog **registry** this PRD builds on (SoR for Product/SKU/Category/Attribute/`CatalogVersion`, the `bundle` SKU type, metering-unit declaration, and `PlanTier` taxonomy); upstream foundation, not superseded |
| Plan & Price Modeling use case | Legacy use-case format; **superseded** by this PRD for normative requirements. The partner **effective-price** preview (with pricing hierarchy) is **narrowed** here to base list price + overlay disclaimer; an accurate end-customer quote requires Tariffs overlay evaluation — Tariffs MUST pick up a partner-facing effective-price preview requirement (Open Questions) |
| Effective-dating price-windows use case | **Consolidated into this PRD** (windows owned by the pricing gear; the UC doc = scenario source) |
| Tariffs — Commercial Pricing Logic PRD | Downstream consumer; authoritative for formula evaluation and `pricingSnapshotRef` composition |
| Metering & Pricing Module PRD | Metering collection only; pricing hierarchy evaluation in Tariffs |

## 3. Actors

### 3.1 Human Actors

#### Finance Manager

**ID**: `cpt-cf-bss-pricing-actor-finance-manager`

**Role**: Defines plans, prices, tiers, tax-inclusive flags, and submits publishes for approval.
**Needs**: Accurate recurring/usage pricing; audit trail; safe price changes.

#### Product Manager

**ID**: `cpt-cf-bss-pricing-actor-product-manager`

**Role**: Configures add-ons, bundles, and plan composition.
**Needs**: Package deals; compatibility rules; marketplace bundles.

#### Finance Reviewer

**ID**: `cpt-cf-bss-pricing-actor-finance-reviewer`

**Role**: Approves material price/plan publishes under the two-person rule.
**Needs**: Two-person rule; rejection with reason; threshold controls.

#### Partner / System Integrator

**ID**: `cpt-cf-bss-pricing-actor-partner`

**Role**: Previews list prices for end-customers.
**Needs**: Region/currency-scoped catalog base price before Tariffs overlays (requires an explicit catalog-preview read grant).

#### Catalog Admin

**ID**: `cpt-cf-bss-pricing-actor-catalog-admin`

**Role**: Tenant-scoped catalog governance.
**Needs**: RBAC, bulk operations, retirement and migration orchestration. (The scoped historical-import capability this actor used to hold is **struck** — D-330, 2026-08-16.)

#### Auditor

**ID**: `cpt-cf-bss-pricing-actor-auditor`

**Role**: Reads price history and approval records.
**Needs**: Immutable history; export; no PII in events beyond policy.

### 3.2 System Actors

#### Catalog Registry (Product & SKU)

**ID**: `cpt-cf-bss-pricing-actor-catalog-registry`

**Role**: SoR for Product/SKU/Category/Attribute/`CatalogVersion`, the `bundle` SKU type, metering-unit declaration, and `PlanTier` taxonomy; the **sole** incrementer of `CatalogVersion`.

#### Rating (incl. the evaluation core — former "Tariffs / PLAL")

**ID**: `cpt-cf-bss-pricing-actor-rating`

**Role**: The consolidated rating gear (rating ADR-0002 — the former Tariffs/PLAL consumer merged into this actor). Its **evaluation core** consumes the published read model (model kind, tiers, evaluation-policy fields, derived-meter definition, reserved rate, `customerGroup` overlays), evaluates commercial prices, and composes `pricingSnapshotRef`; its **pipeline** consumes events and warmed read models, resolves `{skuId, planId, priceId}` and evaluation-policy fields deterministically, and never reads draft state.

#### Subscriptions

**ID**: `cpt-cf-bss-pricing-actor-subscriptions`

**Role**: Sells eligible plans; owns the plan-change boundary/mode (`changeEffectiveAt`, `changeMode`), plan-change classification, trial runtime, entitlement enforcement, `PlanLink` migration, and sellability checks from the published inputs (proration math is evaluated by the rating gear).

#### Billing

**ID**: `cpt-cf-bss-pricing-actor-billing`

**Role**: Consumes billing descriptors via `CatalogVersion`; derives deferral policy from `billingTiming`; posts immutable invoices.

#### Marketplace

**ID**: `cpt-cf-bss-pricing-actor-marketplace`

**Role**: Consumes bundle rev-share rules for fee accrual.

#### Tax Engine

**ID**: `cpt-cf-bss-pricing-actor-tax-engine`

**Role**: Determines tax scheme (VAT/GST/US sales tax) and maps `region` -> tax jurisdiction; consumes `taxInclusive` display basis and `taxCategory` reference (no scheme/calc here). Post-MVP dependency.

#### Promotions

**ID**: `cpt-cf-bss-pricing-actor-promotions`

**Role**: Owns coupon/discount authoring and evaluation; provides the external discount instrument that `discountRef` resolves against.

#### Contracts

**ID**: `cpt-cf-bss-pricing-actor-contracts`

**Role**: Supplies contract locks and negotiated RI-style reservation rates (per-account overlay); contract-locked subscriptions are excluded from scheduled migration.

## 4. Operational Concept & Environment

### 4.1 Module-Specific Environment Constraints

- **Multi-tenant isolation**: plans, prices, and price overlays are tenant-scoped; brand/region are scoped per IdP claims at the gateway. Pricing `region` is decoupled from the authorization region claim.
- **Time**: all effective dating, window boundaries, `grandfatherUntil`, `availableFrom`/`availableTo`, and anchor math are **UTC** (per manifest), at **millisecond resolution** (**D-144**, 2026-08-02 implementation finding). This line fixed the zone and left the resolution open, while `cohort` — a scope-key axis derived from a cutover instant — is matched for **equality** against a window bound produced in another gear (Rating's D-126 bootstrap), where an unstated quantum makes a grandfathered generation unfindable. An authored instant carrying finer precision **MUST** fail validation (`TIMESTAMP_PRECISION_EXCEEDED`, 422) rather than be truncated: truncation silently moves the instant a scope-key axis, a window bound and an approval-time floor are all derived from, and a truncating producer agrees with a non-truncating consumer until the day it does not, with no failure in between. Storage bookkeeping no operator authors — `created_at`, audit-chain and outbox timestamps — is outside the rule: it enters no contract field and is compared with nothing.
- **Determinism / snapshot**: published plans expose a complete, frozen read model; consumers resolve via `pricingSnapshotRef` and MUST NOT read draft state or substitute defaults for absent evaluation-policy fields. Posted invoice periods MUST NOT re-query mutable catalog rows.
- **No charge computation**: the catalog persists and publishes structure; it MUST NOT compute monetary charges, evaluate overlays, or perform FX. Mathematical formulas belong in Tariffs.
- **Precision**: amount precision follows the **ISO 4217 minor unit** for the row's currency (0 for JPY/KRW, 2 default, 3 for BHD/KWD/OMR); a flat 2-decimal cap MUST NOT be assumed.
- **Region / brand taxonomy**: `region` and brand-scoped `PriceOverlay` `brand` values MUST be members of the tenant's configured taxonomies, validated before publish.

**Event alignment (manifest §4.1)**

- **Producers (frozen event-name contract — names are normative; no aliases or renames without a versioned contract change)**: `PlanCreated`, `PlanUpdated`, `PlanPublished`, `PlanRetired`, `PlanMigrationScheduled`, `PlanPublishDegraded`, `BundleUpdated`, `PriceCreated`, `PriceUpdated`. Conditional emission governs *when* an event fires, not *whether* the name set is stable.
- **Manifest §4.1 alignment (additive)**: manifest §4.1 lists these producers additively alongside the pre-existing `PlanCreated/Updated`, `BundleUpdated`, `CatalogVersionPublished`, and `PriceWindowScheduled/Activated/Expired`. `CatalogVersionPublished` (registry) is **consumed** here, not re-declared; the `PriceWindow*` set is **produced by this PRD's design** since the window consolidation (frozen manifest names preserved).
- **Consumers (downstream)**: Rating (read-model warm), Subscriptions (eligibility + `PlanMigrationScheduled` -> `PlanLink` creation), Tariffs (policy warm), Billing (descriptor cache), Marketplace (bundle/listing).

## 5. Scope

### 5.1 In Scope

> Priority mapping: HIGH -> `p1`, MEDIUM -> `p2`, LOW -> `p3`. Detailed requirements are carried as functional requirements (§6), acceptance criteria (§12), and normative appendices (§17).

| **Feature** | **Priority** | **Notes** |
|-------------|--------------|-----------|
| Plan definition: one-time, recurring (monthly/quarterly/semiannual/annual/custom), usage-based | `p1` | Draft -> published; parent SKU published |
| Hybrid plans (recurring base + usage) | `p1` | Same `planId`; publish validation |
| Per-seat (per-unit recurring) plans | `p1` | `modelKind=per_unit`; `quantitySource=subscription_seat_count\|manual`; quantity resolved by Subscriptions |
| Custom billing frequency | `p2` | `quarterly`, `semiannual`, `customEveryN{Days\|Months}(n)`; interval `n` validated (> 0) |
| Plan composition and mandatory `PlanTier` | `p1` | Manifest §4.1; `PlanTier` taxonomy + SKU-level value owned by registry; enforced at plan publish here |
| Plan phase price schedules (catalog read model) | `p1` | trial/interim/evergreen price mapping + ordering + `convertsToPhaseId`; Subscriptions owns trial runtime |
| Proration input contract publication | `p1` | `billingAnchorPolicy`, `prorationBasis`, `creditOnDowngrade` on recurring rows; Subscriptions computes proration |
| Billing timing (`billingTiming`) on recurring rows | `p1` | `in_advance \| in_arrears`, required at publish, frozen; Billing deferral policy |
| Plan-change contract publication | `p1` | `allowedChangeTargets` + `comparabilityRank`; Subscriptions classifies upgrade/downgrade/switch |
| Plan sellability gate + `availableFrom`/`availableTo` (all cycles) | `p1` | Purchase blocked without an active window + committed version; unified purchasability dating; joint with Subscriptions |
| Optional one-time setup/activation charge | `p2` | First-class one-time row on recurring/hybrid plans, charged once per subscription lifetime (at activation; at trial conversion for trialed plans; never re-charged on migration) |
| Entitlement grant set in published read model | `p1` | Feature flags/quotas (or `PlanTier`-resolved ref); Subscriptions provisions/enforces |
| Base and regional price rows (multi-currency) | `p1` | ISO 4217; `region` scope; `brand` via brand-scoped `PriceOverlay` (not a price-row axis) |
| Price overlay authoring (`PriceOverlay`) | `p1` | Author/validate scope (partner/orgTier/brand/region/**customerGroup**/global), adjustment (markup/discount/fixed), explicit precedence. Precedence/stacking **evaluation** remains in Tariffs |
| Customer-group segment pricing (`customerGroup` `PriceOverlay` scope) | `p1` | A per-plan adjustment **line set** per group overlay (D-42; per line percent, or per-currency amounts — D-08) resolved via `payerTenantId`, most-specific line per `(plan, sku)`; region variance via region-scoped base rows; BSS-owned group taxonomy + effective-dated audited membership; resolved group frozen in snapshot; membership changes audited; immediate re-resolution or bulk group moves = material change. Different per-group tier *structures* stay Future |
| Usage pricing tiers with explicit model kind | `p1` | graduated vs volume; non-overlapping bands |
| Package (block) pricing (`modelKind=package`) | `p2` | `packageSize`/`packagePrice`; round-up math in Tariffs |
| Level-based usage aggregation (`aggregationFunction` `sum`\|`peak`\|`time_weighted` + `aggregationGranularity`, `maxHold`) | `p1` | D-44; granule fold evaluated by Rating (T-D-17); non-`sum` publish gated on the joint `level-aggregation` fixture |
| Included allowance (`includedAllowance {quantity, rolloverPolicy}` on usage rows, publish-compiled) | `p1` | D-45: `none` → `$0` first band + frozen marker; `carry` → D-43 per-period promotional grant (Billing executes drawdown); `sum` rows only |
| Trailing-tier qualification (`tierQualificationWindow` `current`\|`trailing_period`) | `p2` | D-40; prior anchor-derived period qualifies the rate tier; the locked rate is pinned per subscription per period by Rating — **not** frozen in `pricingSnapshotRef` (D-60); Rating supplies the trailing aggregate; publish gated on the `trailing_tier` joint fixture |
| Prepaid credit grant (catalog definition) | `p2` | Grant fields frozen in snapshot; balance/drawdown owned by Billing/Rating (External dependency, GA gate) |
| Reserved-capacity price (self-service reserved rate + on-demand) | `p1` | `reservedRate`/`reservationFlavor` on the usage row; Tariffs evaluates (step 6) sourcing the rate from the snapshot |
| Interim `discountRef` day-1 discount hook | `p2` | Referential-integrity only; external discount instrument; evaluation in Tariffs/Promotions |
| `tierAggregationWindow` and `billingGranularity` on usage prices | `p1` | `billingGranularity` required on all usage rows; `tierAggregationWindow` on tiered usage rows; consumed by Tariffs |
| Tax-inclusive / tax-exclusive price flag per row | `p1` | Display and downstream tax basis; no tax calc here |
| Billing descriptor set per Plan/SKU | `p1` | Invoice template, tax category, GL, itemization |
| Add-on rules (dependency, bounds, overrides) | `p1` | Compatibility validation |
| Bundle definition (SKUs, constraints, rev-share, itemization, price basis) | `p2` | Marketplace; `sum_of_parts` vs `own_price` |
| `PriceWindow` ownership, linkage and publish-time coverage check | `p1` | Window store/state machine/activation + `PriceWindow*` events owned here (consolidated); linkage + validation here |
| `priceEligibility` / grandfathering on price rows (incl. `grandfatherUntil` bound) | `p1` | Tariffs step 2 consumes; renewal re-bind via Subscriptions |
| Catalog publish contract (events + read model) | `p1` | `PlanCreated/Updated/Published/Retired` |
| Consumer read-model resolution contract | `p1` | Tariffs/Rating deterministically resolve published inputs; no draft read, no default substitution |
| Publish fan-out atomicity / degraded handling | `p1` | Retry-to-SLO or `PlanPublishDegraded`; no charge against partially-published plan |
| Plan publish validation (fail-closed) | `p1` | Meters, tiers, descriptors, hybrid, PlanTier |
| Plan approval workflow (two-person rule) | `p1` | Thresholds; reject with reason |
| Tenant policy objects (approval threshold, tax-display) | `p1` | Fail-safe defaults: two-person rule if no threshold; fail-closed tax-display |
| Plan retirement and subscriber migration | `p1` | Grandfathering, scheduled cutover, alt plan |
| Migration idempotency & cancellation | `p2` | Re-trigger-safe; cancel-before-effective without affecting already-migrated subs |
| Legacy subscription snapshot synthesis | `p2` | Synthesize+freeze `pricingSnapshotRef` for subs lacking one (`migrated-origin`) |
| Plan versioning and immutable price history | `p1` | New price row on change; audit; see Price-change mechanisms (§17.5) |
| Contract-locked plan protection | `p1` | Reject structural mutation |
| Effective catalog price preview | `p2` | Base list price; Tariffs for full hierarchy. Draft-plan bill simulation needs a draft/sandbox extension (Open Questions) |
| Plan clone / duplicate | `p2` | Draft copy with new ids |
| Bulk price import/update | `p2` | All-or-nothing **validation**; **per-row commit** with conflict report |
| Mass repricing (idempotent, deduplicated events) | `p2` | Re-run-safe bulk adjustment with throughput SLO |
| Mutation idempotency keys | `p2` | Plan/Price create/update accept client idempotency key; duplicate returns original |
| Price history view and export | `p2` | Auditor/Finance |
| One-time min/max purchase quantity | `p2` | min/max qty (purchasability dating unified into the all-cycles `availableFrom`/`availableTo` row above) |
| Operator UX (wizard, tier editor, migration UI) | `p2` | Frontend DESIGN |
| RBAC: CatalogAdmin, FinanceReviewer, ProductManager, Auditor | `p1` | Tenant/brand/region scoped |
| Data residency for price/audit data per jurisdiction | `p2` | Aligns with jurisdiction-configurable audit retention NFR |

### 5.2 Out of Scope

- **Tariff evaluation formulas**, override hierarchy, coupon stacking, FX math — Tariffs. This includes **evaluating the derived (composite) meter formula**: this PRD persists the composite definition (units, formula-as-data, output unit); Tariffs computes the result.
- **`PriceWindow` scheduler UI** — the Frontend DESIGN (the window backend — store, activation job, events — is in scope here since the consolidation).
- **`PriceOverlay` precedence/stacking evaluation and contract override evaluation** — Tariffs + Contracts. (`PriceOverlay` **authoring/validation** is in Scope; see §5.1.)
- **Subscription state machine, plan-change boundary/mode + runtime, trial runtime mechanics, entitlement enforcement, plan-change-mid-cycle, downgrade entitlement-overflow enforcement, `PlanLink` execution** — Subscriptions; **proration math** — the rating gear (consumes `(changeEffectiveAt, changeMode)`). This PRD publishes the **inputs**; downstream executes.
- **Cross-currency / cross-region / cross-frequency mid-cycle plan changes** — NOT supported at launch (Future scope); handled as cancel + new subscription.
- **Coupon / discount / promo-code authoring** and coupon **evaluation/stacking** — Promotions domain / Tariffs. Neither coupon authoring nor evaluation is a catalog concern here.
- **Refunds, credit/debit notes, refund/credit eligibility execution** — Billing / Payments. Catalog does not flag per-row refundability (Future).
- **Prepaid credit balance ledger, drawdown, zero cut-off, auto-recharge execution** — Billing / Rating. Catalog only **defines** the grant (frozen in snapshot); balance is never tracked here.
- **PSP card rails, payment gateway integration, ERP/GL export** — Payments / Billing. Catalog supplies `glCode`/descriptors via `CatalogVersion` only.
- **Usage ingestion, `UsageRecord` shapes** — OSS / Metering.
- **Tax-scheme determination (VAT/GST/US sales tax), tax calculation, invoice posting, GL posting** — Tax Engine / Billing. Catalog owns only the tax **display basis** (`taxInclusive`) and `taxCategory` reference.
- **API schemas, proto, error codes** — Design doc(s).
- **Full ASC 606 recognition** — Finance/Billing; catalog MAY carry SSP reference tags in Future scope.
- **Self-service term / auto-renew metadata** (`termLength`/`autoRenew` on the Plan) — NOT a catalog concern at MVP; Billing's Catalog-term default is deferred with it (Future scope).

## 6. Functional Requirements

> **Content boundary**: FRs define WHAT the catalog MUST persist, validate, and publish, not data models or APIs. Concrete schemas, proto definitions, error taxonomies, and mathematical formulas are owned by the corresponding DESIGN. Heavy normative reference tables (billing-cycle requirements, price-structure kinds, model-kind conformance, plan-composition rules, price-validation rules, price-change mechanisms, consumer contracts, and advanced primitives) are preserved verbatim in §17.

### 6.1 Plan Definition and Billing Cycles

#### Supported billing cycles

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-billing-cycles`

The catalog **MUST** support one-time, recurring, usage-based, and hybrid billing cycles with the per-cycle catalog requirements in §17.1. A Plan **MUST** reference a **published** SKU and be storable in `draft` before publish — and, because it is storable incomplete, publish **MUST** reject a Plan that has declared **no billing cycle** and a recurring/hybrid Plan that has declared **no frequency** (`CYCLE_METADATA_MISSING`, the field named — **D-149**, 2026-08-03: every §17.1 rule is conditioned on the cycle, so an undeclared cycle satisfied the whole matrix by having nothing to test). Publish **MUST** also reject a sold `(currency, region)` that carries no base price row of the `chargeKind` the cycle mandates — no `one_time` row on a one-time Plan, no `recurring` row on a recurring or hybrid one (`BASE_MARKET_INCOMPLETE`, naming cycle, `chargeKind` and market — D-149, the base-side sibling of `USAGE_MARKET_INCOMPLETE`). Recurring frequency **MUST** support `monthly`, `quarterly`, `semiannual`, `annual`, and `customEveryN{Days|Months}(n)`.

**Rationale**: The full commercial-cycle matrix is the foundation for IaaS/PaaS/SaaS and marketplace selling.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Custom billing frequency

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-custom-frequency`

For a recurring Plan the catalog **MUST** persist the interval as metadata for `quarterly`, `semiannual`, or `customEveryN{Days|Months}(n)` and **MUST** reject a non-positive `n`. A `customEveryN Days(n)` cycle **MUST** anchor on `subscription_start` (a `calendar_month`/`fixed_day` anchor **MUST** fail publish); a `customEveryN Months(n)` cycle **MAY** anchor on `subscription_start` or `calendar_month` — a `subscription_start` day beyond the target month's length clamps to its **last day** (the same rule as `fixed_day`), with the **anchor day preserved** across periods (each period clamps independently: Jan 31 → Feb 28 → Mar 31, no drift; all UTC; joint anchor fixture with Subscriptions). `n` **MUST NOT** exceed the configured custom-interval cap.

**Rationale**: Custom cycles must be explicit and anchor-compatible to rate deterministically.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Hybrid plan completeness

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-hybrid-completeness`

A hybrid Plan **MUST** declare both a recurring base price row and a usage price row on the same `planId`; publish validation **MUST** reject a hybrid missing either part, and the read model **MUST** expose both rows under one `planId` for Tariffs hybrid evaluation. The usage part is required **per sold market** (D-84): every `(meter, dimensionKey)` line the plan prices **MUST** have a published usage row in every `(currency, region)` where a recurring row exists (`USAGE_MARKET_INCOMPLETE` otherwise) — a hybrid must never be sellable in a market whose usage events would fail closed; a market where usage is genuinely free authors an explicit `$0` row, never an absence.

**Rationale**: Hybrid economics require both components present and co-resolvable under one plan identity.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

#### Per-seat (per-unit recurring) pricing

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-per-seat`

A **non-usage** `modelKind=per_unit` row **MUST** persist a unit price and a `quantitySource` (`subscription_seat_count` | `manual`; and, for `manual`, the fixed quantity); on a `per_unit` **usage** row `quantitySource` is **absent** — the meter supplies `Q` and `billingGranularity` is required like on every usage row (§17.1; the 2026-07-28 kind×chargeKind fix — this FR's unqualified wording had lagged it). The catalog **MUST NOT** infer, meter, or compute the quantity (zero metering-unit footprint); the read model **MUST** let Rating/Tariffs resolve the per-period quantity from the declared source.

**Rationale**: Per-seat pricing must be explicit about quantity provenance without metering it.

**Actors**: `cpt-cf-bss-pricing-actor-subscriptions`

#### Optional one-time setup/activation charge

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-one-time-setup`

Recurring and hybrid plans **MAY** declare an optional one-time setup/activation price row (`chargeKind=one_time_setup`) on the same `planId`, charged **once per subscription lifetime** (at activation; for a plan with a `trial` phase — at entry into the first non-trial phase; never charged on plan change or `PlanLink` migration, whether or not the origin plan carried a setup row) and frozen in `pricingSnapshotRef`. It **MUST** be a first-class plan price row (participating in approvals, snapshot, preview) — **not** a synthetic add-on SKU — and publish **MUST** validate it as one-time (no recurrence, no `billingTiming`/tier fields).

**Rationale**: Setup fees are common and must be first-class, not modeled as fake add-ons.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Plan-level minimum fee / cap per period

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-period-floor-cap`

A Plan **MAY** declare a **period floor** and/or a **period cap** per sold `(currency, region)` — *"this plan bills at least X, and at most Y, per period in this market"* — frozen in `pricingSnapshotRef` and published on the read model. The catalog **authors, validates and freezes**; it **MUST NOT** evaluate: Rating resolves the amount, currency and attachment scope from the pinned snapshot and emits a `PeriodFloorCapObligation`, and **Billing** executes `max(total, floor)` / `min(total, cap)` over the period's aggregated total after step 9 (rating `fr-period-floor-cap-obligation`, §6.11/§17.2). The **attachment scope is not authored**: a bound published on the plan subject is plan-level by construction, which rating §17.2 already reads as `recurring+usage` (**D-319**).

The market pair is the bound's **key**, and the currency of the market is the denomination of both amounts — no second currency field, and no implicit FX. Publish **MUST** reject a bound authored on a `(currency, region)` the plan prices nothing in (`PERIOD_FLOOR_CAP_MARKET_UNSOLD`, the market named), and **MUST** reject a bound that admits no bill — a `0` floor, a `0` cap, a floor above its cap, or a row authoring neither (`PERIOD_FLOOR_CAP_AMOUNT_INVALID`, the offending bound named). **Absence is the only spelling of "no minimum"** (D-319): the per-line non-negative guard already holds every line at or above zero before the floor applies, so `max(total, 0)` is a no-op and a `0` would be a second spelling of one state. A sold market carrying **no** bound is the ordinary plan and is never a violation.

This is **not** a quantity floor — `minQtyThreshold` / `minQtyUsage` bound a quantity and this bounds money; conflating them is what rating §6.2 forbids. It is **not** a negotiated committed-spend pool either: those are Contracts' system of record (`commitmentPools[]`, `TrueUpObligation`, rating T-D-14) and carry a true-up this does not. The bound is **not cohort-scoped**: it is a fact about the plan revision, so a grandfathered generation is answerable to the revision's bound at the version its subscription is pinned to (D-319).

**Rationale**: "Pay at least $X/month" is a routine commercial ask, and the rating boundary that executes it has been reserved since §17.2 was written; only the catalog authoring field was missing.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`, `cpt-cf-bss-pricing-actor-rating`

### 6.2 Price Structure and Model Kinds

#### Explicit model kind

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-model-kind`

Each price row **MUST** persist an explicit `modelKind` (`flat` | `per_unit` | `graduated` | `volume` | `package`) with no implicit default at rating time; a tiered row with unspecified model kind **MUST NOT** publish. The catalog **MUST NOT** compute charges — Tariffs applies the formula per the conformance mapping (§17.2). The **kind × chargeKind matrix** (D-18, completed by the 2026-07-28 review fix and propagated here by D-57) is total: `flat` and `per_unit` are valid on **non-usage** rows; `per_unit`, `graduated`, `volume`, `package` are valid on **usage** rows. `graduated`/`volume`/`package` on a `recurring`/`one_time`/`one_time_setup` row, **and `flat` on a `usage` row**, MUST fail publish (`MODEL_KIND_CHARGEKIND_MISMATCH`) — the tier machinery presupposes a metered quantity stream, and the plain untiered metered rate is `per_unit` on a usage row, not `flat`. Tiered per-seat pricing is Future scope.

**Rationale**: Rating divergence is prevented only when the model kind is explicit and frozen.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

#### Tier band validation

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-tier-validation`

Under the `[fromQty, toQty)` convention, tier bands **MUST** be ascending by `fromQty`, non-overlapping, and contiguous (no gaps); the top band **MUST** be **open-ended** (`toQty=null`) — a closed top band **MUST** fail publish. "Price undefined above X" is never the commercial intent: quantity capping is owned by entitlement **quotas** (Subscriptions enforces), per-period fee caps are Tariffs Future scope, and a different price above X is simply another band — so any quantity is always rateable on a tiered row.

**Rationale**: Off-by-one, overlap, and gap errors at band edges cause silent mispricing.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

#### Package (block) pricing

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-package-pricing`

A `modelKind=package` row **MUST** persist `packageSize` (> 0), `packagePrice` (>= 0), and — like every other non-linear kind — `tierAggregationWindow`, the window over which `used` accumulates **before** block round-up (**D-58**, propagated by D-70; absence fails publish, `EVAL_POLICY_MISSING`), with tier-band fields **absent**; publish **MUST** reject otherwise. `billingGranularity` quantizes the quantity but does **not** bound a period, so it does not supply the accumulation window: 150 units folds to `ceil(150/100) = 2` blocks under `invoice_period` and to 30 blocks under a daily reading — a 15x spread on one published row. The read model **MUST** expose `packageSize`/`packagePrice`/`tierAggregationWindow`; Tariffs computes the round-up math (§17.2) and the catalog **MUST NOT** compute the charge.

**Rationale**: Block pricing is a distinct model that must be authorable and gated on a joint fixture.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

#### Model kind / Tariffs formula conformance

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-model-kind-conformance`

The catalog `modelKind` enum and the Tariffs formula matrix **MUST** reconcile one-to-one per §17.2; publish of any `modelKind` lacking a **joint golden conformance fixture** **MUST** be blocked. `package` (repeating-block) and `per_unit` (external-quantity) **MUST** each have a joint golden fixture before publish.

**Rationale**: A single kind-to-formula source of truth prevents catalog/Tariffs divergence.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

#### Price amount and currency validation

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-price-amount-validation`

A price amount **MUST** be >= 0 (`0` valid for free tiers, `trial`/`interim` phases, and the first graduated band; negatives rejected), currency **MUST** be valid ISO 4217, and precision **MUST** follow the currency's ISO 4217 minor unit. The catalog **MUST NOT** perform FX: if no row exists for a requested `(currency, region)`, preview/publish **MUST** fail closed (no base-currency fallback unless an explicit `currencyFallbackPolicy` is configured — Future).

**Rationale**: Financial correctness requires non-negative, correctly-scaled, FX-free catalog amounts.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Level-based usage aggregation

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-level-aggregation`

A `usage` price row **MAY** author `aggregationFunction` (`sum` (default) | `peak` | `time_weighted`) and, for non-`sum` rows, `aggregationGranularity` (`hour` (default) | `day`) — level-based billing per D-44; Rating owns the granule-fold evaluation (rating T-D-17: `Q` = Σ per-granule folds, additive by construction) and the catalog **MUST NOT** compute a fold. A non-`sum` row **MUST** declare `maxHold` (`max_hold_granules`, integer >= 1 — no default: the sampling-gap bound is a commercial statement, authored explicitly, fail-closed); beyond `maxHold` granules the level reads 0 and an operator signal raises (fail-visible, never guessed — rating-side execution). Publish **MUST** fail on: either field present on a non-usage row or carrying an unknown value, `maxHold` missing/`< 1` on a non-`sum` row, or `maxHold` present on a `sum` row (`LEVEL_FIELDS_INVALID`); a non-`sum` row whose meter is not level-shaped (collector `gauge` kind) or whose SKU-declared billable unit does not match `level unit × granule` for the declared granularity (`LEVEL_UNIT_MISMATCH`); a non-`sum` row whose `billingGranularity` is not the counterpart of its `aggregationGranularity` — `hour` ⇒ `per_hour`, `day` ⇒ `per_day`, every other pairing rejected (`LEVEL_GRANULARITY_MISMATCH`, **D-77**: on a level row the granule fold **is** the quantization, so without the pairing the tier-band unit is `level × granule` under one rule and the `billingGranularity` unit under another — a 24x divergence at the band edge that no other check sees); a non-`sum` row on a derived (composite) meter (`LEVEL_COMPOSITE_FORBIDDEN` — composite inputs stay window-sum at launch). All three fields are frozen in `pricingSnapshotRef`, and publish of any non-`sum` row is blocked without the green joint `level-aggregation` fixture (AC #60 mechanics; granule fold, late-sample re-fold, `maxHold` gap).

**Rationale**: The launch product set bills on levels (cloudlet peak-per-hour, storage GB-month); the commercial rule must live in the catalog, not the usage emitter (D-44).

**Actors**: `cpt-cf-bss-pricing-actor-rating`

### 6.3 Plan Composition, Descriptors, and Phases

#### Mandatory PlanTier

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-plantier-mandatory`

Every Plan **MUST** declare `PlanTier` before publish (optional at draft); the taxonomy and SKU-level value are owned by the registry. Publish **MUST** validate that the Plan's `PlanTier` **equals its parent SKU's `PlanTier`** unless an explicit, audited override is declared (no silent divergence), so downstream reads one unambiguous effective tier. A **post-publish** registry change of the SKU's tier **MUST** flag every affected published plan as tier-divergent in the read model (remediated by re-publish or an audited override); consumers keep resolving the frozen published tier — divergence is a remediation signal, never a silent retro-change.

**Rationale**: Subscriptions/SLA/quotas depend on one unambiguous, present tier.

**Actors**: `cpt-cf-bss-pricing-actor-subscriptions`

#### Meter injectivity

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-meter-injective`

A usage plan **MUST** carry **exactly one priced line per `(meter, dimensionKey)` per scope-key slice** (`currency`/`region`/`priceOverlay`/`phase`/`priceEligibility`/`cohort` legitimately multiply rows); a **duplicate** line within one slice is the ambiguity that **MUST** fail publish. A plan **MAY** price **several** `meteringUnit`s (**D-103**, 2026-07-31 review fix — the earlier "each usage plan revision MUST map **exactly one** `meteringUnit`" was contradicted by `fr-hybrid-completeness`'s per-`(meter, dimensionKey)` market rule, by `fr-prepaid-credit-grant`'s `applicability` **set** of the plan's usage meters, and by the enforcing partial `UNIQUE`, which carries both `meter` and `dimension_key` and therefore only ever implemented the per-line reading). A **derived (composite) meter** (one output unit) remains the way to price several **units as one billable line**, and separate single-meter SKUs composed via bundle/add-ons remain available — neither is the only multi-meter path.

**Rationale**: Injective meter mapping prevents line collisions and ambiguous rating.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

#### Add-on rules

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-addon-rules`

Add-on SKUs **MUST** be published and compatible with the base SKU; dependency/conflict edges are **plan-authored** on the add-on rules (referencing other add-ons of the same plan's set). Dependency **cycles** **MUST** fail publish; a `conflicts_with` pair fails publish **only when both add-ons are marked required** (an unsatisfiable set) — otherwise the declared conflict is a valid selection-time constraint Subscriptions enforces at order assembly (rejecting an unconditional pair at publish would make declaring a conflict impossible at all). An add-on rule's quantity bounds **MUST** admit a selection — a required add-on **MUST** have `maxQty >= 1`, `minQty` **MUST NOT** exceed `maxQty` when both are set, and `stepQty` **MUST** be positive when set — each failing publish as `ADDON_QTY_RANGE_INVALID` with the offending bound named (**D-150**, 2026-08-03: only the first was ever written, and only as a storage check §17.4 named no code for, so its violation reached the author as an internal fault while an inverted range or a zero step published an add-on nobody can take). A **recurring-only** add-on **MUST NOT** be attached to a one-time base plan (`ADDON_INCOMPATIBLE` — **D-149**: §17.1 stated the ban with no code and no owner, and it is decided from the add-on SKU's own published plans, which only this rule reads). An optional price override reference **MUST** be persisted on the plan snapshot. The override reference **MUST** resolve to a **published price row of the add-on SKU's own plan** (a normal row with its own scope key, windows, and coverage), and it **binds that row's canonical scope-key *family* modulo the market axes** (D-97, amended by **D-116**, 2026-07-31: a single canonical key fixes one `(currency, region)` and cannot itself cover a multi-market base plan — the pre-D-116 wording was unsatisfiable): the bound object is the key with `(currency, region)` free; the subscriber's bound market selects the member, and resolution at `t` follows **that member** through windows, so a supersession's successor legitimately serves the override — the frozen mapping never dangles on a routine price change; base-plan publish **MUST** validate a published, covering **member** exists for every `(currency, region)` the base plan sells — a detached override amount on the base plan is not authorable.

**Rationale**: Constrained, acyclic add-on composition keeps subscription assembly valid.

**Actors**: `cpt-cf-bss-pricing-actor-product-manager`

#### Bundle composition

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-bundle-composition`

A Bundle **MUST** declare its price basis (`sum_of_parts` or `own_price`), reference **published** `includedSkuIds`, and (for `sum_of_parts`) reference component **`planId`s** whose rows cover **every** `(currency, region)` the bundle sells in (matching `frequency` for recurring components) — a missing/ambiguous component row **MUST** fail publish. A component `planId` **MUST NOT** itself be a `bundle`-type plan (flat composition at launch; bundle nesting is Future scope), and **MUST NOT** be a phased plan (`COMPONENT_PHASED` — composition semantics for phased components are a named Future gate). On every sold `(currency, region)` all component rows — and the bundle's own rows for `own_price` — **MUST** carry the **same** `taxInclusive` display basis (`BUNDLE_TAX_BASIS_MIXED` otherwise, **D-119**, 2026-07-31 review fix: D-110 pinned one basis per market per *plan* because an invoice is one document, and a bundle composes several plans onto exactly one invoice — with the D-54-pattern reverse guard: a component re-publish that would mix a referencing bundle's market fails the same way). Component `one_time_setup`/`one_time` rows never charge under a bundle purchase (setup is tied to standalone activation; the bundle's own setup row is the only activation charge). Rev-share (authorable on `sum_of_parts` bundles only — D-55) **MUST** sum to 100% per included vendor SKU with an explicit per-group platform cut and a nominated primary party **within each vendor-SKU group** (default = the platform) absorbing that group's rounding residual within <= 0.01% (1 bp) tolerance — publish normalizes the absorber's effective share to an exact split (typed values audited); `invoiceItemization` (`aggregate`|`itemize`) **MUST** be persisted and preserve per-SKU rev-share for Marketplace accrual. Bundle **creation**, component add/remove/replace, any rev-share change, a `price_basis` change and an `invoiceItemization` change **MUST** be **always-material** (**D-104**, 2026-07-31 review fix): materiality is computed as per-row deltas over **price rows** and a `sum_of_parts` recomposition produces none, so with a threshold configured a component swap or a re-split published with **no** approver — the hole D-50 closed for overlay lines, at comparable stakes (a rev-share split is vendor payout; a component swap changes what the customer receives at an unchanged price), and the reason D-11's "the composition is protected at publish time by the approval content pin" only holds if such a publish is material.

**Rationale**: Marketplace bundles must be unambiguous, currency-covered, and rev-share-reconciled.

**Actors**: `cpt-cf-bss-pricing-actor-marketplace`

#### Billing descriptor completeness

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-billing-descriptors`

Publish **MUST NOT** proceed without the complete billing descriptor set per manifest §4.1 / D-48 v1 — **three descriptor-set fields** (invoice line template, GL code, composition/itemization rules) plus **two row-borne elements**, both `pricing_price` columns rather than descriptor-set fields: `billingTiming` on each recurring row (2026-07-28) and `taxCategory` as each row's tax-category reference (**D-110**, 2026-07-31 review fix — the category is per row, so a single per-plan descriptor column could not hold it); the v1 contract's **content** is unchanged. The validation report **MUST** list any missing element, naming the row for the row-borne ones. For `taxCategory` the required element is the row's **effective** category — `coalesce(row.tax_category_ref, region readiness default)` — which publish **MUST** resolve and freeze with the row, and a row whose effective category resolves to nothing **MUST** fail publish regardless of the tenant tax-display policy (**D-154**, 2026-08-03: the policy's warn mode governed that arm, so a pinned v1 element could be published absent; and freezing only the raw column left Billing re-resolving the fallback against the tenant-declared region taxonomy, a mutable row, which is exactly what this FR's sufficiency clause forbids). The set **MUST** be sufficient for Billing/ERP to post without re-querying mutable catalog rows; extension is additive-only (D-48) and the extended required-set is declared **per tenant in `pricing_policy_object`** (**D-152**, 2026-08-03 — "config-extensible without a schema change" had named no surface anywhere in the design set, so the validator could only ever check the pinned three).

**Rationale**: Invoice reproducibility requires a complete, frozen descriptor set at publish.

**Actors**: `cpt-cf-bss-pricing-actor-billing`

#### Plan phase price schedule

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-plan-phases`

For a phased plan the read model **MUST** map each phase id to its price row references, persist phase ordering, each phase's kind (`trial` | `interim` | `evergreen` — **D-358**), each phase's optional operator `displayName` (**D-357**, presentation only) and each phase's `convertsToPhaseId` successor, reject a dangling or cyclic `convertsToPhaseId`, and require **exactly one terminal phase**. On a plan whose billing cycle carries a **recurring part** (`recurring` or `hybrid`), every phase **MUST** be covered by ≥ 1 published **recurring** price row for every `(currency, region)` the plan sells — an uncovered phase fails publish (`PHASE_UNCOVERED`: a phase conversion must never resolve to nothing). One-time and usage-only plans are **outside** this rule (D-74): they carry no recurring rows at all, so a literal reading would block every one of them via their implicit terminal phase; the "sold but unrateable" state is guarded for them instead by the immutability of the terminal `phase_id` across revisions (`inst-ph-terminal-stable`, D-64) together with the row-level window-coverage check. **Usage rows are phase-invariant by default**: one usage row covers all phases, and an explicit phase-scoped usage row overrides it for its phase (phase-specific wins — a published resolution rule adopted verbatim by Tariffs, joint fixture; e.g. free trial usage = an explicit trial-phase usage row at 0). A phase-scoped override **MUST** have the phase-invariant terminal-phase row of its `(meter, dimensionKey)` line — an orphan override fails publish (`PHASE_OVERRIDE_ORPHANED`, **D-117**: without the base the unit guard has no comparison target, per-market completeness never sees the line, and after conversion the line resolves to nothing — "sold but unrateable" through the override door; phase-limited-only metering is a named Future gate) — and **MUST** preserve that row's unit/counter-determining fields (`modelKind` — authored and, since D-45's compile, **presented** (**D-317**) — `billingGranularity`, aggregation/qualification windows, `packageSize` — `PHASE_OVERRIDE_UNIT_MISMATCH` otherwise, D-89 + D-122 + D-317): the tier counter is phase-blind and a phase conversion never resets it, so the continued `Q` must keep its denomination **and its pricing formula** across the conversion. Every **non-terminal** phase **MUST** publish `phaseDurationDays` (> 0; publish fails on a non-terminal phase without a duration or a terminal phase with one); for a `trial` phase the catalog **MUST** publish `displayTrialDays` (= its `phaseDurationDays`) for preview/quoting, and **only** a `trial` phase carrying that duration may declare one — a `displayTrialDays` on an `interim` or `evergreen` phase, or on any phase with no `phaseDurationDays` to project, **MUST** fail publish (`DISPLAY_TRIAL_DAYS_INVALID`, the phase named — **D-151**, 2026-08-03: the equality was carried by a storage check alone, which is silent on the phase kind and which SQL's NULL propagation satisfies whenever the duration is absent, so a Plan with no trial phase could publish a trial length that preview quotes and Subscriptions enforces). Subscriptions enforces phase runtime from these same published values (single source).

**Rationale**: Phase->price resolution and trial length must be catalog-authored and acyclic; runtime stays in Subscriptions.

**Actors**: `cpt-cf-bss-pricing-actor-subscriptions`

### 6.4 Multi-Currency, Regions, and Tax Display

#### Multi-currency price rows

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-multi-currency-rows`

A Plan **MUST** support independent price rows per `(currency, region)` linked to the same `planId`, emitting `PriceCreated` per row. The system **MUST** support **at least 20 currencies per plan** as a guaranteed floor and hold the read SLO at that floor.

**Rationale**: Global selling requires many first-class per-market rows without FX derivation.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Region and brand taxonomy validation

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-region-brand-taxonomy`

A price row's `region` **MUST** be a member of the tenant's configured region taxonomy; an unknown region **MUST** fail validation before publish. `brand` is **not** a price-row field: a **brand-scoped `PriceOverlay`**'s `brand` **MUST** be a member of the tenant's configured brand taxonomy or fail validation.

**Rationale**: Uncontrolled region/brand values corrupt scope resolution and overlay matching.

**Actors**: `cpt-cf-bss-pricing-actor-catalog-admin`

#### Tax display basis

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-tax-display-basis`

The catalog **MUST** persist the `taxInclusive` display basis and `taxCategory` reference only (no scheme/calc), **both on the price row** — the row's `taxCategory` reference is the sole source of truth for a row's category and the billing-descriptor **set** carries no tax-category field (**D-110**, 2026-07-31 review fix: a per-plan descriptor column cannot mirror a per-row value, and the promised publish-time consistency check was undefined whenever two rows of a plan differed by line or by region; `taxCategory` now rides the row exactly as `billingTiming` does — the D-48 v1 content is unchanged). Every published row of a plan on one `(currency, region)` **whose `priceEligibility` is not `existing_grandfathered`** **MUST** carry the **same** `taxInclusive` value; a mixed-basis market **MUST** fail publish (one invoice, one display basis — the rule is per market, so a plan tax-exclusive in US and tax-inclusive in EU stays legal). Grandfathered generations are **excluded** from the uniformity set (**D-132**, 2026-08-01 review fix): they are immutable, never superseded and never unpublished, so including them let one cutover freeze a market's display basis permanently, with every later publish failing on a row nobody could fix; their subscribers read the basis from their own frozen snapshot. A `taxInclusive=true` row without region tax readiness **MUST** be governed by the tenant tax-display policy (default **fail-closed**). A row with **no effective tax category at all** — neither `tax_category_ref` on the row nor a default in the region taxonomy — **MUST** fail publish unconditionally, outside that policy (**D-154**, 2026-08-03: `taxCategory` is a **pinned** D-48 v1 descriptor element whose absence `fr-billing-descriptors` makes a publish-blocking MUST, and a per-tenant display policy may not publish past a pinned contract element; the two arms had been one sentence and read as one switch, while one is a readiness signal about an engine outside this gear and the other is a contract element the catalog promised Billing). The **resolved effective** category — `coalesce(row.tax_category_ref, region default)` — is what freezes into the read model and the snapshot, exactly as the resolved `rounding_policy_ref` does, so a consumer never re-derives the fallback from a taxonomy row a tenant can re-declare under a frozen `CatalogVersion` (D-154). Region tax readiness (`taxCategory` + rate-present marker) is **tenant-declared per `(tenant, region)` configuration at MVP** and is verified against Tax Engine once it GAs (a divergence flags affected rows for re-publish, never a silent retro-change). Because Tax Engine is post-MVP, a `taxInclusive=true` **price row MAY** be authored but **MUST** be flagged **not-sellable-GA** until Tax Engine GA — the flag applies **per price row / `(currency, region)` market**, gating only the tax-inclusive markets of a mixed plan, not the plan as a whole; MVP sells tax-exclusive.

**Rationale**: Display basis is a catalog concern; tax scheme/calc and jurisdiction mapping are gated on Tax Engine.

**Actors**: `cpt-cf-bss-pricing-actor-tax-engine`

#### Single-currency-per-invoice binding

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-invoice-currency-binding`

Publish/preview **MUST** reject the enumerated configurations that would force mixed-currency lines onto a single invoice: (i) a **required** add-on, or an add-on **price-override target**, lacking a covering published row for a **`(currency, region)` pair** the base plan sells (per pair, not per currency — D-95: the currency-only reading left a required add-on missing the bound market to die at order assembly) — an **optional** add-on's coverage gap **MUST NOT** block the base plan's publish, because an add-on that covers no row in the bound currency is simply not attachable on that market: the gap is enforced at **attachment** time by Subscriptions against the sellability read, and an unattachable optional add-on can never put a second currency on the invoice (D-73; the design-side rule is `inst-cb-addon`); (ii) a `sum_of_parts` bundle whose component rows do not cover every currency the bundle sells; (iii) an `own_price` bundle without a matching-currency component set. Currency selection at activation is owned by Subscriptions; the pricing `region` likewise **binds once at activation** — resolved by Subscriptions from the payer's commercial profile (never client-supplied) and frozen with the currency in `pricingSnapshotRef`. The single-currency-per-invoice invariant is enforced here.

**Rationale**: A subscription must resolve all lines in one bound currency; mixed-currency invoices are invalid.

**Actors**: `cpt-cf-bss-pricing-actor-billing`

### 6.5 PriceWindow Linkage and Coverage

#### Publish-time window coverage

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-pricewindow-coverage`

For billable usage at time `t`, an active or scheduled `PriceWindow` **MUST** exist for the resolved **canonical scope key** (resolved on the **base** `priceOverlay`) or publish **MUST** fail (no silent fallback), directing the operator to schedule a window. Because `priceEligibility` and `chargeKind` are part of the key, a grandfathered row and its successor, and the components of a hybrid plan, are **distinct keys** that MAY each hold an active window at the same `t` — not an overlap violation.

**Rationale**: Publishing a billable row without window coverage lets Tariffs step 2 resolve nothing (fails closed).

**Actors**: `cpt-cf-bss-pricing-actor-rating`

#### Future-gap coverage across scheduled windows

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-future-gap-coverage`

For two or more scheduled `PriceWindow` rows on one scope key, publish validation **MUST** reject any uncovered time interval (gap) between the end of one active/scheduled window and the start of the next for billable periods, directing the operator to close the gap.

**Rationale**: A coverage gap fails rating closed for everyone during the gap.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Sellability gate

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-sellability-gate`

A subscription/purchase **MUST NOT** be created unless **all six sellability predicates** hold at `t` for **every canonical scope key the purchase binds on the bound `(currency, region)`** — the recurring/usage/`one_time_setup` components and every phase of the chain, eligibility-resolved, grandfathered generations excluded; one failing component key blocks the plan-market, never a partial sale (D-94, joint rule with Subscriptions — the bundle conjunction below is the bundle-specific statement of this same rule): (1) an **active** (not merely scheduled) `PriceWindow` covers the key **and the key's active-plus-scheduled coverage extends through `now + the longest billing cycle sold on the key`** (D-80 — an open-ended window satisfies this trivially; a finite-covered key stops selling one full cycle before its coverage ends, so no purchase can land in a trailing void; the read model exposes the per-key coverage end); (2) the plan is addressable in a **committed `CatalogVersion`** (pending fan-out / unwarmed read model is not sellable); (3) plan-level `availableFrom`/`availableTo`, when set, are open at `t`; (4) the plan's lifecycle state is not retired; (5) no GA gate flags the bound market (per-market `not_sellable_ga`, prepaid-execution gate); (6) for a **standalone** line, the offered SKU carries the registry `sellable = true` flag (products gear, D-46) — bundle-**component** references are exempt from (6). The read model **MUST** expose all six predicates; Subscriptions enforces the conjunction. The gate governs the creation of **new** subscriptions only: a renewal of an existing subscription is not a purchase and **MUST NOT** be blocked by it (retirement and passed purchasability dates never kill in-flight renewals; those follow the grandfathering/migration mechanics). For a bundle, the gate evaluates the **conjunction over its components**: the bundle is sellable at `t` only while every referenced component key passes predicates (1)–(5) — components are exempt from (6), which applies to the bundle SKU itself (plus the bundle's own purchasability dates). Optional plan-level `availableFrom`/`availableTo` (the unified pair for **all** billing cycles), when set, **MUST** be validated against window coverage at publish; deferred publish ("publish at T") is out of launch scope.

**Rationale**: Selling before a plan is both windowed and rateable produces a first rating that resolves no window.

**Actors**: `cpt-cf-bss-pricing-actor-subscriptions`

#### Grandfathering eligibility

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-grandfathering-eligibility`

The read model **MUST** expose `priceEligibility` (`all_subscriptions` | `new_subscriptions_only` | `existing_grandfathered`), the row's `cohort` (generation), and any `grandfatherUntil` (UTC) for Tariffs step 2; new subscriptions after cutover **MUST NOT** bind to a grandfathered row, and a bound subscription renewing on/after **its generation's** `grandfatherUntil` **MUST** be signalled no longer eligible so Subscriptions re-binds it to the current eligible row (null = indefinite). Generations coexist: each cutover creates a new `cohort` row and prior generations stay live; within the grandfathered class Tariffs resolves the row whose `cohort` matches the subscription's pinned price id. Each generation's `PriceWindow` **MUST** cover through its `grandfatherUntil` **plus the longest billing cycle sold on that key** (open-ended when null) — re-bind happens only at the next renewal after expiry, and the margin keeps every bound period rateable until then; a cutover or window mutation that would violate the bound **MUST** be rejected. The exposed `grandfatherUntil` is a **grandfathered-row field**: it **MUST** be non-null only where `priceEligibility = existing_grandfathered`, and a value on any other row **MUST** fail publish (`GRANDFATHER_UNTIL_FORBIDDEN`, 422 — **D-147**, 2026-08-02 implementation finding). Published unqualified, the field read as a general per-row sell-until date that a consumer could apply outside the eligibility path, giving one fact three unreconciled homes beside `PriceWindow` `effectiveTo` and the plan's `availableTo`; and because the pairing was only ever a physical column check, an operator who set the bound on the wrong row got an internal fault instead of a refusal naming the field. The converse stays as the glossary states it: a grandfathered row with a null bound is indefinite.

**Rationale**: Grandfathering and new-only eligibility are first-class commercial rules resolved deterministically.

**Actors**: `cpt-cf-bss-pricing-actor-subscriptions`

### 6.6 Price Overlays and Segment (Customer-Group) Pricing

#### PriceOverlay authoring and precedence

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-priceoverlay-authoring`

The catalog **MUST** author and validate `PriceOverlay`s as **adjustment-line containers** (D-42, CONFIRMED 2026-07-28): scope (partner/orgTier/brand/region/customerGroup/global) — every non-`global` scope value validated against its declared universe (**D-120**, 2026-07-31: `brand`/`region` against the Slice-4 tenant taxonomies, `partner`/`orgTier` against the tenant taxonomies D-120 adds — before it those two classes had no value universe anywhere — `customerGroup` against the BSS group taxonomy; a referenced value cannot retire, and the payer → partner/orgTier resolution input Tariffs matches against is a registered needs-decision on the Tariffs contract), **one or more adjustment lines** keyed `(planId?, targetSku?)` — each line with its own type (markup/discount/fixed, whose **effect on the running amount is normative** (**D-138**, 2026-08-01 review fix — the kinds were authorable while their semantics were defined in no document, pricing deferring to Tariffs and the rating step-4 design naming only "percentage" and "absolute amount" lines): `markup` **adds**, `discount` **subtracts**, and **`fixed` replaces** the running line amount — a replacement, because `markup`/`discount` already cover additive absolute money, so an additive `fixed` would be a duplicate; a `fixed` layer therefore discards the layers below it (authoring **MUST** warn when it is not the lowest-precedence matching layer) and **resets** the cumulative-markup accumulator the rating-side composition cap tracks; a percent magnitude is a single basis-points value, **range-bounded** — a `discount` line in `(0, 10000]` bp (a discount above 100% is not authorable) and a `markup` line `> 0` bp, an out-of-range value failing publish with `ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE` naming the line (**D-67**, propagated by D-71: the `>= 0` money constraint binds authored *price rows*, not overlay lines, so without this bound the common "150% of list" data-entry inversion published cleanly); an **amount-based** magnitude is money, MUST be `>= 0` at its currency's ISO 4217 minor unit, and MUST carry **per-currency values covering every currency the line's target scope sells**, no implicit FX; a currency added later flags the line for remediation), a duplicate line key or an out-of-scope line target failing publish, with the **most-specific line winning within a list** (`(planId, targetSku)` > `(planId)` > list-default; the pre-D-42 single-adjustment overlay = the one-default-line case) — and an explicit `precedence`; a line additionally carries an **optional `cohort` eligibility filter** (**D-78**): unset (the default) the line applies **only** to `priceEligibility ∈ {all_subscriptions, new_subscriptions_only}` rows, and a **grandfathered generation is adjusted only by a line explicitly naming its cutover instant** (an unknown `cohort` fails publish, `OVERLAY_LINE_COHORT_UNKNOWN`) — otherwise a single markup line would reprice a cohort whose price the grandfathering machinery guarantees, without mutating any row; the filter selects *which rows a line may touch* and does not alter the most-specific ordering, which runs unchanged within the eligible set; it **MUST** reject a duplicate `precedence` within one scope class. Because uniqueness is per class, cross-class ties are possible: the read model **MUST** publish the normative class-specificity tie-break — `customerGroup > partner > orgTier > brand > region > global` — adopted verbatim by Tariffs, and authoring **MUST** warn on an equal-precedence cross-class pair with overlapping targets. A `PriceOverlay` adjustment **MAY** be effective-dated via its own `[effectiveFrom, effectiveTo)` interval (validated per scope + adjustment target, not on the canonical price-row key), and **MUST** declare its tax basis (or explicitly delegate to Tariffs) or fail publish. Every `PriceOverlay` **MUST** carry a `disclosure` flag — `restricted` (default, fail-closed: the overlay and its existence are never exposed on consumer-facing enumeration or preview and materialize only in the member payer's own evaluation context) or `public` (Presentation / the Tariffs effective-price preview MAY disclose the adjusted price); operator/service reads are unaffected. Precedence/stacking **evaluation** remains in Tariffs. Every committed `PriceOverlay` mutation is a **publish unit through the engine** (§17.5): it becomes consumer-visible only via a committed `CatalogVersion` + read-model warm, within the propagation SLO — never "with the next unrelated plan publish".

> **D-42 (CONFIRMED 2026-07-28)** — supersedes F-88's *single-magnitude* half: one segment deal carries different rates per plan inside **one** overlay (no *N* sibling overlays with hand-managed precedence). Still strictly adjustment-only — different tier *structures* per group stay Future / separate-plan (that half of F-88 stands). Cross-overlay semantics unchanged: class rank + per-class precedence stack exactly as before; per-currency coverage (D-08) and referential integrity/dangling-on-retire (D-31) re-attach per line. The Pricing Studio line grid is the reference UX.

**Rationale**: Deterministic overlay ordering and unambiguous tax basis require authored, validated precedence.

**Actors**: `cpt-cf-bss-pricing-actor-catalog-admin`

#### PriceOverlay referential integrity

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-priceoverlay-referential-integrity`

A `PriceOverlay` row whose scope references a plan or SKU that is not published **MUST** be rejected with a clear business error and **MUST NOT** be exposed in the read model.

**Rationale**: Overlays referencing draft/unpublished targets would resolve against non-existent base rows.

**Actors**: `cpt-cf-bss-pricing-actor-catalog-admin`

#### Customer-group segment pricing

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-customer-group-pricing`

The catalog **MUST** author a `customerGroup`-scoped `PriceOverlay` (a **line set** per D-42 — each line `(planId?, targetSku?)` with its own percent or per-currency-amount magnitude (D-08), most-specific line wins; region-differentiated group pricing composes from the region-scoped base rows, never a region axis on a line), own the **group taxonomy** (BSS-governed, validated at authoring), and publish an **effective-dated, audited membership** record on the payer's commercial profile (resolved via `payerTenantId`) into the read model — **at most one active membership per payer across all groups** (a conflicting enrollment is rejected; a transfer is one atomic audited **move**), so the resolved group is always unique; the **resolved group** **MUST** be frozen in `pricingSnapshotRef`. Every committed membership mutation is a **publish unit through the engine** (§17.5) — consumer-visible only via a committed `CatalogVersion` + warm (registry batching coalesces bulk enrollments), so a renewal after the commit always sees the membership. A membership change is renewal-aligned by default (immediate re-resolution is an explicit material change); a group discount/move affecting many payers is a **material change**, and all membership changes **MUST** be audited. Entirely different **tier structures** per group are out of launch scope. Overlay evaluation is owned by Tariffs.

**Rationale**: Segment pricing must be governed, audited, and deterministically frozen without changing tenant topology.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

### 6.7 Publish Validation, Approval, and Events

#### Fail-closed publish validation

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-publish-validation-failclosed`

Publish **MUST** be blocked (and `PlanPublished` **MUST NOT** be emitted, nor Rating read models warmed) when validation detects any invalid condition in the aggregate fail-closed set (§17.4 price-validation rules and §12 AC #26), including ambiguous meter, overlapping/gapped tiers, unspecified `modelKind` on a tiered row, evaluation-policy fields on a non-usage row, invalid `package`/prepaid config, missing `PlanTier`/descriptors, invalid hybrid, currency precision over the ISO 4217 minor unit, unknown region/brand, add-on dependency cycle, uncovered window gap, a recurring row missing `billingTiming`, a non-one-time setup row, or a `minQtyThreshold` with no declared floor type (or a `usage` floor with no declared fallback).

**Rationale**: A single aggregate fail-closed gate prevents any ambiguous configuration from reaching production.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Two-person rule and segregation of duties

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-approval-two-person`

A **material change** (above the configured threshold, or a first publish with no baseline) **MUST** require **one independent approver** before `PlanPublished` (submitter + 1 approver = two distinct principals); the submitter **MUST NOT** satisfy the approval slot, and any self-approval attempt **MUST** be rejected and audit-logged. The submission **MUST** pin the exact submitted content (content hash); any mutation of the subject while pending **MUST** void the approval (back to draft, fresh submit), and an approval decision **MUST** verify the pinned content — a reviewer can only approve exactly what they reviewed. The approver's authorization scope **MUST** cover every region/brand the pinned change touches. Submitter and approver identities and timestamps **MUST** be logged; a rejection returns the Plan to `draft` with reason and notifies the submitter.

**Rationale**: Unauthorized or self-approved pricing change is a financial-fraud risk.

**Actors**: `cpt-cf-bss-pricing-actor-finance-reviewer`

#### Approval-threshold policy

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-approval-threshold-policy`

The tenant approval-threshold policy **MUST** express materiality as an absolute amount or percentage delta **per currency**; for a multi-currency change each affected row's delta is compared in its **own** currency and the rule trips if **any** row exceeds its threshold. **The delta domain is defined per model kind (D-115, 2026-07-31 review fix — "the row's delta" previously had no operand for the rows that carry tiered revenue, whose `amountMinor` is NULL by construction):** `flat`/`per_unit` → the amount delta; `graduated`/`volume` → the band-wise unit-price vector, comparable **only while the band geometry (bounds and count) is unchanged**; `package` → the package-price delta **only while `packageSize` is unchanged**. Any change to a **quantity-determining/geometry field** — band bounds or count, `packageSize`, `manualQuantity`, `includedAllowance.quantity` — is **material regardless of thresholds** (no effective-price delta is computable catalog-side; a `manualQuantity` 10 → 1000 multiplies the charge at zero amount delta), as is a percent-only evaluation against a zero baseline. **A mutation with no computable price delta is always material (D-115, generalizing the G1 fail-safe):** row contract fields (`billingTiming`, `prorationBasis`, `billingAnchorPolicy`, `creditOnDowngrade`, `taxInclusive`/`taxCategory`, `quantitySource`) and plan-shape revision content (descriptors incl. the GL code, phase graph/durations, the add-on rule set, cycle/frequency, availability dates, the `PlanTier` override, `invoiceGroupingKey`, and the plan-change contract — `usageCounterOnPlanChange` (D-113), `allowedChangeTargets`, `comparabilityRank`) — a pure-shape revision carries zero price rows, so per-row evaluation alone could never trip on it. Mutating the threshold policy is itself **always material** (direction-agnostic): the change **MUST** take effect only after an independent second approver confirms it through the standard approval workflow — the two-person rule's foundation is never single-person-editable; in-flight submissions keep their submit-time materiality. The system **MUST** apply the two-person rule by default and **MAY** auto-publish with no independent approver **only if** a threshold is explicitly configured **and** the change is below it **and** it is not a first publish. A row with no prior baseline of its own (a new currency/region/phase/chargeKind key added to a published plan) has no computable delta and is therefore **always material** (fail-safe). **Every `PriceOverlay` mutation that changes an adjustment or its audience** (creation; line add/remove; magnitude or kind change; scope/precedence/dating/disclosure change) is likewise **always material** (D-50, 2026-07-28): an overlay line is not a price row — there is no per-currency baseline delta to threshold, and percent lines carry no currency — so the G1 fail-safe applies and the mutation routes through the approval workflow before its publish unit commits.

**Rationale**: Fail-safe materiality (two-person rule unless explicitly below an configured threshold) prevents silent large changes.

**Actors**: `cpt-cf-bss-pricing-actor-finance-reviewer`

#### Publish fan-out atomicity and degraded handling

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-publish-fanout-atomicity`

Once the version commits (`CatalogVersionPublished`), the system **MUST** either complete read-model warming within the 5s SLO via retry or mark the publish degraded and emit `PlanPublishDegraded`; it **MUST NOT** leave a state where charges can resolve against a partially-published plan. The pre-commit batching delay (`PlanPublished` -> `CatalogVersionPublished`) is governed by the max batching-delay SLO, not by `PlanPublishDegraded`. **The catalog MUST record when it learned the version had committed, and evaluate both clauses against that instant (D-166, 2026-08-03, found while building the read side):** the finalize that would stamp the commit instant never runs on the path where the warm is failing, so without a separately recorded observation the only measurable duration is the publish's own age and the 5s rule marks **every** publish degraded, including every one behaving exactly as the batching SLO budgets. The degraded state is the derived predicate (unresolved, commit observed, older than the SLO) and is stored in no column: completion ends it by making the predicate false, which is what "consumers observe completion" already means for this gear.

**Rationale**: Partial publish must never expose a rateable-but-incomplete plan.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

#### Frozen event-name contract

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-event-contract`

The catalog **MUST** emit `PlanCreated`, `PlanUpdated`, `PlanPublished`, `PlanRetired` (and, conditionally, `PlanMigrationScheduled`, `PlanPublishDegraded`, `BundleUpdated`, `PriceCreated`, `PriceUpdated`) with the frozen names in §4 (no aliases or renames without a versioned contract change), ordered per `(tenantId, aggregateId)` where the manifest applies, carrying correlation/idempotency keys so consumers can dedupe (at-least-once delivery).

**Rationale**: Downstream cache warming and eligibility depend on a stable, ordered, dedupable event contract.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

#### CatalogVersion increment on publish

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-catalogversion-increment`

On every `PlanPublished` the plan's content **MUST** become addressable in a `CatalogVersion` with the registry as the **sole** incrementer; the registry **MAY batch** multiple approved publishes into one discretionary catalog publish (not one dedicated version per `PlanPublished`). `PlanPublished` carries a **pending** version reference; the committed `CatalogVersion` is finalized when the registry emits `CatalogVersionPublished`, and `pricingSnapshotRef` **MUST** pin that committed version (§17.5).

**Rationale**: Determinism requires pinning the exact committed version the registry batches the plan into.

**Actors**: `cpt-cf-bss-pricing-actor-catalog-registry`

#### Consumer read-model resolution and monotonicity

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-consumer-readmodel-resolution`

Consumers **MUST** resolve `{skuId, planId, priceId}`, model kind, ordered tier bands, and evaluation-policy fields exactly as published, from the committed `CatalogVersion`, **without** reading draft state and **without** substituting any default for an absent evaluation-policy field (absence must have failed publish). The read model **MUST** be **monotonic per `CatalogVersion`**, and a version becomes **pin-eligible** only once its `CatalogVersionPublished` has fired, **every subject row that version projects carries its warm-completion marker** (**D-101**, 2026-07-31 review fix: with the marker per subject row and per-subject fallback to the prior delta, a *single* pin resolved one plan pre-change and another post-change and a replay of that pinned run produced different money — pin-eligibility restores stability by construction, and "the newest completed version" below regains a referent it lost when the marker moved off the version), **and every earlier version is itself pin-eligible** (**D-114**, 2026-07-31c review fix: pin-eligibility is a **prefix-closed frontier** — per-subject resolution reaches *through* the pin into older deltas, so without the prefix a stuck older version's late warm made one pin resolve a subject at two different versions over time, the D-101 divergence one version out). **Both quantifiers range over the tenant's own refs (D-164, 2026-08-03, found while building the read side):** `CatalogVersion` is minted by a cross-tenant registry, so a tenant's committed versions are a sparse subset of the global sequence — read globally, no tenant's frontier could ever advance past a version another tenant's publish consumed, and one tenant's unwarm subject would hold another tenant's pin. A rating run **MUST** pin one **pin-eligible** `CatalogVersion` for its entire duration, and at pin time the pinned version **MUST NOT** lag the newest pin-eligible version by more than 5s; a version that fails to become pin-eligible within the max batching-delay SLO **MUST** raise a Critical alarm and consumers **MUST** keep resolving the previous pin-eligible version coherently rather than a partially-warmed mix.

**Rationale**: Deterministic, monotonic resolution is the basis of reproducible rating.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

#### Pricing snapshot on billable artifacts

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-pricing-snapshot`

Charge artifacts and `BillableItem`s **MUST** carry catalog refs and `pricingSnapshotRef` per manifest §4.1; posted invoice periods **MUST NOT** re-query mutable catalog rows. Publish **MUST** stamp identifiers sufficient for the manifest `pricingSnapshotRef`, and the catalog-side stamp is **three** parts: the **pending `CatalogVersion` ref**, the **resolved price ids**, and the **evaluation-policy version**. That third part is the **evaluation-policy generation** (**D-162**, 2026-08-03): a declared constant of this gear, format `ep-<n>`, naming which evaluation-policy field set the frozen row content is to be read under, bumped whenever a decision adds a field to that set, removes one, or moves a field across its boundary — and **not** bumped when a rostered field's requiredness, values, default or meaning changes, so it tracks the shape of the evaluation input rather than its semantics. The roster, the bump rule and the append-only generation log are normative in [`design/01-foundation.md`](./design/01-foundation.md) §4.4, which the gear's build guard reads. Publish stamps a real generation or **nothing** — the ban on a placeholder (**D-161**, 2026-08-03) is unchanged and is why the generation is declared rather than invented per publish: Rating's snapshot composer fail-closes on a missing pricing pre-stamp and never fabricates a segment, so an absent value refuses at the boundary that owns the check while an invented one satisfies it, rides posted periods immutably, and can never afterwards be told from a real version. **Rating's adoption is owed**: its composer must read the segment and compare it for equality only, treating an unrecognised generation as a fail-closed refusal rather than a value to interpret.

**Rationale**: Reproducibility and posted-financial immutability require a complete frozen snapshot ref.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

### 6.8 Plan Lifecycle: Versioning, Retirement, Migration

#### Plan versioning and immutable history

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-plan-versioning`

A price or tier change on a published Plan **MUST** version the Plan and create **new** immutable `Price` records (prior rows retained as history); existing subscriptions **MUST** continue on their frozen snapshot until renewal or migration. Subscriptions bound via `existing_grandfathered` are the exception: they are **live-resolved** against an immutable grandfathered row (§17.5).

**Rationale**: Immutable price history and frozen snapshots protect active subscribers from silent repricing.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Published rows are append-only

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-published-rows-append-only`

A **published** price row on the canonical scope key **MUST NOT** be deleted (whether or not subscriptions are bound); published rows are append-only history. The operator **MUST** use supersession + grandfathering (or retirement + migration) instead. Deletion is available **only** for `draft` rows that were never published.

**Rationale**: Quotes, previews, exports, and overlays reference published rows; there is no deletion event to fan out.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Supersession within a scope key

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-supersession`

Supersession is versioning scoped to one canonical scope key: it **MUST** create a new immutable `Price` row (never mutate in place) and open/close the corresponding `PriceWindow` rather than overlap it, operating within one `priceEligibility` class, one `cohort`, and one `chargeKind`. It executes only as the atomic **supersession unit** (D-88): successor row + predecessor-window shorten + successor-window schedule composed gap-free under **one** approval unit and one transaction — no primitive-by-primitive path exists — and the changeover instant **MUST** be strictly future at submit and at least the max batching-delay SLO (D-47) in the future at approval commit (`SUPERSESSION_INSTANT_PASSED`; a mass-repricing run names one such instant for all its rows), so a successor window never activates before its row is addressable in a committed `CatalogVersion`. On a **usage** row the successor **MUST** preserve the unit/counter-determining fields — `meter`, `dimensionKey`, **`modelKind` (D-98)**, `billingGranularity`, `aggregationFunction`, `aggregationGranularity`, `tierAggregationWindow`, `tierQualificationWindow`, **`packageSize` (D-122)** and, since D-45's compile, the **presented** `modelKind` an `includedAllowance` moves on an untiered `per_unit` row (**D-317**: introducing or dropping the declaration mid-window changes the formula pricing the continued counter while every authored field reads identical) — because the tier counter `Q` continues across supersession and must keep its denomination **and its pricing math** (a `per_hour` → `per_day` successor would apply an hours-counted `Q` to day-denominated bands; a `graduated` → `volume` flip would re-price the already-accumulated window total under new math; a mid-window `packageSize` change re-buckets the accumulated `used` — block math is non-linear in the window and block counting presupposes one size per window, while `packagePrice` remains the legitimate price lever); a change fails publish (`SUPERSESSION_UNIT_MISMATCH`, D-82 + D-98 + D-122 + D-317 — changing what or how a key meters, or which formula prices it, is structural and routes through plan revisioning + migration). At most **one pending approval unit** (supersession or grandfathering cutover) may exist per canonical scope key at a time — a concurrent second submission **MUST** be rejected with a conflict. An `existing_grandfathered` row is **immutable in price** and **MUST NOT** be superseded; the only permitted mutation is setting or **tightening** `grandfatherUntil` (never loosening, never the price), which is a material change.

**Rationale**: Scope-key-bounded supersession preserves non-overlap and grandfathered-price stability.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Plan retirement

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-plan-retirement`

Retirement **MUST** block **new** subscriptions to that `planId`, preserve existing subscription snapshots, emit `PlanRetired`, and **trigger the window-cancellation flow** (owned by this PRD's design since the consolidation; emitting `PriceWindowCancelled` per cancelled not-yet-active window, driving its cache-eviction path) — **but only for scope keys with no in-flight subscribers (D-51, 2026-07-28; the per-key count resolves through the §9.2 inbound lane 3 — D-79 — re-resolved inside the retirement commit, fail-closed on outage: windows kept)**: a scheduled window that is the **continuing coverage** of a key with in-flight subscribers (e.g. a supersession successor extending past the active window's `effectiveTo`) **MUST** be kept, not cancelled — retirement stops **selling** (via the sellability gate's lifecycle predicate), never **rating** coverage. It **MUST NOT** merely mark windows invalid. The operator dry-run/confirm **MUST** label kept windows distinctly from cancelled ones before confirm. Retirement **MUST** be rejected (references enumerated) while the plan is referenced as a bundle component or as an add-on price-override target, or while an active contract references the plan (D-108); the referencing composition is remediated first. Retirement **MUST** be an **always-material** change requiring an independent approver in **every** case (**D-109**, 2026-07-31 review fix — it was registered material only when a live cutover unit had to be unwound): it cancels every not-yet-active window of its zero-subscriber keys in one call, which is the act D-62 made two-person for a *single* window; it stops all new sales for the plan; and it is **irreversible** — there is no `retired → published` transition and the plan's open draft revision is closed with it. That closure is a **flip, not a deletion** (**D-145**, 2026-08-02, found while building the draft-authoring plane): the open draft revision row moves to the terminal `abandoned` `lifecycle_state` — its revision-scoped child copies dropped, the flip audited exactly as the deletion it replaces was — because `revision` is an **identity** and a number minted for a plan is never minted again. Deleting the row returned `max(revision)` to its pre-draft value, so the next opened draft minted the **same** number and `(plan_id, revision)` — the durable name `pricing_plan_grant` is keyed by, that every revision-scoped child copies under, and that the audit trail records — denoted two distinct rows over a plan's lifetime; a client holding the discarded revision's row version then satisfied its precondition against the *new* row of that name, which is the lost update `fr-concurrent-edit` exists to refuse, arriving through the key instead of the version. A dry-run confirm screen is not a second principal. Retirement **MUST** also be a **publish unit through the engine** (**D-128**, 2026-08-01 review fix): the commit requests a pending `CatalogVersion` and re-projects the plan subject, with the plan's lifecycle state a projected field, because the sellability gate's lifecycle predicate is required to be evaluable from the *pinned* read model — before it, retirement requested nothing, §17.5's increment table carried no retirement class, and since a retired plan can never publish again no later unit could correct the lag, so the read model advertised a retired plan as sellable permanently. The projected plan subject correspondingly sources the plan's **current** revision, `published` **or** `retired`, so in-flight subscribers keep a resolvable delta.

**Rationale**: Safe retirement blocks new sales while preserving in-flight subscribers and cleaning future windows.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Scheduled migration

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-scheduled-migration`

Scheduled migration to a published target Plan **MUST** emit `PlanMigrationScheduled` for Subscriptions to create effective-dated `PlanLink`s without posted invoice mutation. Retry **MUST** be idempotent (no duplicate `PlanLink` requests for already-processed subscriptions). Cancellation **MUST** be possible for a `scheduled` **or partially-executed** migration (halting further processing; already-migrated subscriptions unaffected; only a completed run is uncancellable), and propagation is a **state handshake**: Subscriptions re-reads the schedule state before beginning (and while continuing) execution. At execution start the contract-lock set and boundary deltas **MUST** be re-resolved against fresh state (a lock is never broken however stale the schedule). A migrated subscription enters the target's **first non-trial phase** — a migration never grants a new `trial`. Scheduling **MUST** validate the subscriber notice period: `effectiveAt` **MUST NOT** be earlier than the schedule instant plus the tenant's configured enforced-migration notice period, whose **default floor is 60 days** (D-49, 2026-07-28); a shorter lead time is rejected (`MIGRATION_NOTICE_TOO_SHORT`, 422, naming the earliest permissible `effectiveAt`).

**Rationale**: Migration must be re-trigger-safe, cancellable, and free of posted-invoice mutation.

**Actors**: `cpt-cf-bss-pricing-actor-subscriptions`

#### Migration safety deltas and legacy snapshot synthesis

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-migration-safety`

Migration configuration **MUST** exclude contract-locked subscriptions (reported, lock never broken) and surface entitlement deltas and add-on deltas (subscribers whose add-ons become invalid, or who lack a required add-on) as blocking deltas. It **MUST** also surface **boundary deltas** as blocking: a target lacking a published row for a subscription's frozen `(currency, region)` pair with matching frequency (cross-currency/region/frequency moves are cancel + new, never in-place); subscribers bound to a grandfathered row are surfaced informationally (the migration ends their legacy pricing). For a legacy subscription with no `pricingSnapshotRef`, the system **MUST** synthesize and freeze a snapshot from the published plan state **as of the per-trigger instant (UTC), frozen at execution** (D-81 — the **migration effective timestamp** for the `migration` trigger, the **earliest unrated usage timestamp** for `first-rating`, per the AC), recorded as `migrated-origin` with a provenance record (source `planId`/revision, resolved price ids, snapshot instant, trigger `migration`|`first-rating`, acting principal, and the **selection tier** per resolved id). The synthesized snapshot is **self-contained** (D-87): it materializes the complete evaluable row content — model kind, tier bands/package fields, evaluation-policy and consumer-contract fields, tax basis, resolved rounding policy — into the frozen payload, and Rating/Tariffs evaluate **from that payload** without resolving its ids through the read model or any `CatalogVersion` (no version is pinned on a `migrated-origin` ref at all — which is what obliges the payload, and is unchanged by D-330's strike of D-76's reference tier, 2026-08-16). Each row **MUST** be selected by an explicit rule (**D-76**, narrowed to one tier by **D-330**), never by window resolution alone: (1) the live or superseded price row whose `PriceWindow` covered the instant — reproducing what rating would have resolved then; (2) otherwise **fail closed** into the exception path — synthesis never guesses and never falls back to the current row. ~~Tier 2, the governed backdated reference row in a store disjoint from published price rows~~, is **struck** (2026-08-16): historical import is out of scope, so a subscription whose price this catalog never authored has no path here and is re-papered onto a plan the catalog publishes. The `first-rating` trigger **MUST NOT** synthesize inline on the rating path: the rating line fails closed into the rating exception path, synthesis runs as a separate audited step, and rating retries against the frozen result — fetched from the **named read surface** `GET /bss-pricing/v1/migrated-origin-snapshots/{subscriptionRef}` under the consuming service identity's `plan × read` (**D-102**, 2026-07-31 review fix: because the ref resolves through no `CatalogVersion`, the read-model contract cannot deliver it, and until this surface the payload consumers are required to evaluate from was exposed by no endpoint, no contract lane and no read model). `migrated-origin` is the **only** per-subscription snapshot the catalog composes — the named exception to Tariffs-composes-the-snapshot, exceptional because there is no live plan state or version for Tariffs to compose from, only this catalog's own append-only published history to resolve the instant against.

**Rationale**: Migrations must not break contract locks, overflow entitlements, or leave subscriptions un-snapshotted.

**Actors**: `cpt-cf-bss-pricing-actor-subscriptions`

#### Contract-locked plan protection

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-contract-locked-protection`

While an active contract references a Plan revision, a **new** revision of that plan **MUST NOT** drop or re-shape what the referenced revision exposes to the contract's subscriptions (the priced `(meter, dimensionKey)` line set, the phase chain, the required add-on set, the descriptor set) and the plan **MUST NOT** be retired — directing the operator to contract expiry; contract-locked subscriptions **MUST** be excluded from scheduled migration. **Price movement on the plan's keys is explicitly NOT blocked** (**D-108**, 2026-07-31 review fix): the negotiated **rate** is Contracts-owned and resolves ahead of the catalog rate, the frozen snapshot covers the in-flight period, and blocking supersession/repricing/cutover instead would let one contracted account freeze a whole market's reprice. The pre-D-108 wording ("structural plan mutation of the referenced revision") was unenforceable in both directions — a published revision row is immutable in content by construction, and price rows attach to `planId`, not to a revision, so locking a revision protected no price.

**Rationale**: Contract-locked commercial terms must not be mutated out from under an active agreement.

**Actors**: `cpt-cf-bss-pricing-actor-contracts`

#### Concurrent-edit conflict detection

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-concurrent-edit`

A submit with a stale version/ETag **MUST** be rejected with a conflict requiring refresh; an interactive edit targeting a row held under an in-flight bulk-import optimistic lock **MUST** fail with a conflict naming the bulk operation. The system **MUST NOT** silently overwrite either change.

The token travels in a **required `If-Match` request header** and a client idempotency key in a **required `Idempotency-Key`** one (**D-171**, 2026-08-03 implementation finding): every API table in the design set carries an Idempotency column reading `ETag` or `client idempotency key`, and until this decision no document named a request **header** anywhere, so a client generated from the set sent neither and discovered both as a 400. On a **plan** route the token names the **revision** it was read from as well as that revision's version (**D-170**, 2026-08-03 implementation finding): the route resolves to the plan's open draft revision when it holds one and to its current revision otherwise, and those counters are unrelated — a freshly opened successor stands at the same initial version as a first draft — so a tag carrying only a version satisfied the precondition against a revision it was never read from, with **no race at all**. That is this requirement's lost update arriving through the revision instead of the version, and the refusal is the same `STALE_VERSION`. The token is **opaque** to the caller: it is copied back verbatim and never constructed or parsed. **And the comparison is made inside the transaction that performs the mutation** (**D-176**, 2026-08-03 implementation finding): a version compared before that transaction opens is a hint rather than a precondition, because the state it read can move in between — which is what happened on the one arm of the plan `PATCH` whose write is an **insert** (opening a successor revision copied from the current one), where a concurrent publish left the successor copied from a revision the caller never read while the call answered as though its precondition had held. For an update that requirement is the compare-and-swap the design already names; for an insert it is a re-read of the guarding row inside the writing transaction, and the two writes one such call performs are one transaction.

The version token is **per price row**, never derived from the plan's (**D-141**, 2026-08-02 implementation finding): `fr-bulk-price-import`'s per-row ETag conflict that fails **only that row** is not expressible when every row of a plan shares one version. **Every** mutating verb on a `draft` price row **MUST** present it — `PATCH` and **`DELETE`** alike; a mismatch is `STALE_VERSION` (409) and an absent precondition is a malformed request. `DELETE` carried no precondition, so a draft row could be destroyed under an unknown version: the same lost update this requirement closes for `PATCH`, reopened on the one verb that leaves nothing behind to reconcile — what is lost is not the row, which is cheap to recreate, but a concurrent editor's uncommitted work. The rule is scoped to `pricing_price` draft rows; window cancellation is an always-material publish unit whose concurrency is governed by the single-pending-unit rule (AC #112), not by a row token.

**Rationale**: Optimistic concurrency prevents lost updates between bulk and interactive edits.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

### 6.9 Consumer Contracts (Subscriptions / Rating / Billing)

#### Proration input contract

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-proration-input-contract`

Each recurring price row **MUST** publish `billingAnchorPolicy`, `prorationBasis` (the canonical enum, §1.4, adopted verbatim by Tariffs), and `creditOnDowngrade` (source-row semantics on a downgrade, per §17.6; `true` with `prorationBasis = none` **MUST** fail publish as contradictory), frozen in `pricingSnapshotRef`; Subscriptions owns the change boundary/mode, the rating gear the proration math (rating §6.11/§17.2). The three fields **MUST** be uniform across the **non-grandfathered** recurring rows of one plan-`(currency, region)` (D-123, row set scoped by **D-132** — an immutable `existing_grandfathered` generation in the set would freeze the market's cycle clock and credit policy permanently): a phased plan's per-phase recurring rows may vary **price**, never the cycle clock or the proration math — Subscriptions consumes a single `billingAnchor`/`prorationBasis` per subscription, so a mixed market leaves the cycle boundary at phase conversion undefined; publish **MUST** fail the divergent market (`PRORATION_CONTRACT_MIXED_MARKET`), per market, not per plan (`billingTiming` stays per-row — a hybrid legitimately mixes timings). A mid-cycle change crossing currency, region, or billing frequency **MUST** be rejected for in-place proration (handled as cancel + new subscription) and the operator **MUST** be warned that in-place credit is forfeited.

**Rationale**: Deterministic proration requires the same frozen inputs on the catalog and Subscriptions/Tariffs sides.

**Actors**: `cpt-cf-bss-pricing-actor-subscriptions`

#### Billing timing on recurring rows

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-billing-timing`

Every recurring price row **MUST** carry `billingTiming` (`in_advance` | `in_arrears`), frozen in `pricingSnapshotRef`; usage rows are implicitly `in_arrears`. Publish **MUST** fail if a recurring row omits `billingTiming`; a hybrid plan **MAY** combine an `in_advance` base row with `in_arrears` usage rows (Billing derives deferral policy from this).

**Rationale**: Billing's deferral policy depends on an explicit, frozen billing timing.

**Actors**: `cpt-cf-bss-pricing-actor-billing`

#### Entitlement grant set

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-entitlement-grant-set`

The published read model **MUST** include the plan's entitlement grant set (feature flags, quotas) or its `PlanTier`-resolved reference for Subscriptions provisioning; publish **MUST** fail if a referenced feature, quota, or `PlanTier` policy is undefined in the registry. This PRD does **not** define entitlement semantics. A **phased** plan **MAY** publish the grant set **per phase** (a `phase→grant-set map`, mirroring the `phase→price map`; D-41): each phase resolves its own feature flags/quotas so a trial phase can confer smaller caps than evergreen. Absent per-phase entries the grant set is plan-level (`PlanTier`-driven) as before; Subscriptions resolves the grant set for the **active phase at `t`** and enforces it. The catalog publishes the map; it never enforces.

**Rationale**: Subscriptions can only provision/enforce entitlements from a complete, published grant set; time-boxed trials with tighter quotas than the paid phase need the grant set to vary by phase just as price does.

**Actors**: `cpt-cf-bss-pricing-actor-subscriptions`

#### Plan-change contract

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-plan-change-contract`

For a plan that participates in self-service changes, the read model **MUST** publish `allowedChangeTargets` (**explicit** target `planId`s — rule-based targets are not authorable at launch, Future scope) and a `comparabilityRank` so Subscriptions can classify upgrade/downgrade/switch and constrain targets. Publish **MUST** validate every listed target is published **and** itself carries a `comparabilityRank` (`COMPARABILITY_RANK_REQUIRED` otherwise) — ranks form one tenant-wide scale, so an A→B classification is always computable. `PlanTier` is **never** an ordering at launch: the authoritative-published-`PlanTier`-ordering alternative named no artifact and no API, and is §17.8 Future (S6 K4). Every edge's **boundary classification** — `in_place` (the target covers the subscription's frozen `(currency, region)` with matching frequency) or `cancel_plus_new` (crosses a boundary; disclosed to the payer before execution) — is **computed at change time by Subscriptions** from both plans' published facts at its pinned version (D-93, revising D-25's publish-time stamp: a target's re-publish warms only its own read-model delta and the source's published revision is immutable, so a stamped value could never be re-computed and a stale `in_place` would run an in-place change across a boundary; the catalog publishes the **inputs** — each plan's market/frequency facts and the edge list — the same read-time discipline as `comparabilityRank`). An edge whose target is later retired is **inert** — Subscriptions re-checks the target's lifecycle state at change time. Absence of `allowedChangeTargets` **MUST** mean **no self-service change** (fail-safe), not any-to-any; enforcement stays in Subscriptions. The plan **MUST** additionally publish **`usageCounterOnPlanChange` ∈ {`reset` (default) | `carry`}** (**D-113**, 2026-07-31 review fix — flagged for veto · joint with Rating + Subscriptions): the snapshot-frozen flag the rating gear's plan-change boundary consults for tier-`Q` continuity (rating T-D-12 / rating PRD §17.2 named pricing as its home while no pricing document defined it — every mid-window plan change evaluated against an absent flag). Rating routes the **target** plan's frozen value; **absence = `reset`**; `carry` is honoured per shared `(meter, dimensionKey)` line **only** where the D-82/D-98/D-122 unit-field set matches between the source and target rows (a mismatched line resets — a counter never carries across denominations); commitment-**pool** carry stays Rating/Contracts-side pending committed-usage authoring (pricing publishes no pool flag).

**Rationale**: Subscriptions must not hard-code ordering or allow moves Finance never approved.

**Actors**: `cpt-cf-bss-pricing-actor-subscriptions`

#### Rating compatibility contract

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-rating-compatibility`

Published plans **MUST** expose stable `{skuId, planId, priceId}` on all downstream artifacts, persist `modelKind` (and `quantitySource` for `per_unit`, `packageSize`/`packagePrice` for `package`), persist `billingGranularity` on usage rows at publish (tiered **and `package`** usage rows **MUST** additionally persist `tierAggregationWindow` — D-58), and satisfy meter mapping (one `meteringUnit` per usage revision; a derived meter counts as one output unit). No monetary charge is computed here.

**Rationale**: Rating consumes frozen inputs; missing or defaulted fields would break deterministic charging.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

### 6.10 Advanced Pricing Primitives

#### Reserved-capacity price

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-reserved-capacity`

A `usage` price row **MAY** carry `reservedRate` and `reservationFlavor` (`consumption` | `capacity`) as attributes on the single row (not a second row), frozen in `pricingSnapshotRef` (§17.7). Tariffs evaluates the reservation (step 6) sourcing the self-service reserved rate from the catalog snapshot; the reserved/allocated quantity is supplied at runtime and the catalog **MUST NOT** meter, allocate, or compute the charge. On a tiered row the matched/allocated reserved quantity is **excluded** from the tier counter `Q` — only the on-demand remainder enters the bands; the reservation joint fixture **MUST** include a tiered-remainder scenario. On a **non-`sum` row** (`aggregationFunction ∈ {peak, time_weighted}`, D-44) `reservationFlavor = capacity` is the **only** authorable flavor: `consumption` **MUST** fail publish (`LEVEL_RESERVATION_CONSUMPTION_FORBIDDEN`, 422) until per-granule netting semantics are decided — the `Q`-exclusion rule above has no defined meaning against a granule fold (**D-53**, propagated here by **D-68**). Negotiated RI-style rates stay in Contracts.

**Rationale**: Self-service reserved rates need an authoring home without double-pricing the meter.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

#### Included allowance

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-included-allowance`

A `usage` price row **MAY** declare `includedAllowance = { quantity N > 0 (in the row's billable units), rolloverPolicy ∈ {none (default), carry(maxPeriods ≥ 1)} }`, frozen in `pricingSnapshotRef` (D-45; supersedes the F-32 "$0-first-band only" stance). Publish **MUST compile** the declaration: for `rolloverPolicy = none`, into the `$0` first band `[0, N)` of the row's band set **plus** a first-class allowance marker (display "includes N units"; reporting separates included from billed) — evaluation is exactly the existing band math, no rating change; for `carry(maxPeriods)`, into a per-period **promotional grant** scoped to the row's meter under the D-43 machinery (issued free, `applicability` = this meter, expiry encodes the carry horizon; exact grant materialization is Design) — drawdown execution is **Billing-owned** and the catalog holds no balance. Publish **MUST fail** on: `includedAllowance` on a non-`usage` row; a row combining an authored `$0` first band with `includedAllowance` (double-free); a non-`sum` row (`aggregationFunction ≠ sum` — level-meter allowance is Future: the boundary in level·granule units varies with period length); an unusable `quantity` — `≤ 0`, **or one whose offset saturates the authored ladder** (2026-08-15, D-317: the same code, because the operand an author acts on is the same declared quantity); a `package` row (no band set to compile into), a **`flat`** row (it prices no quantity, so it counts none) **or a `volume` row** (`ALLOWANCE_KIND_UNSUPPORTED` — catalog `volume` is Variant A, so the selected band's rate applies to the **total** `Q` and a `$0` first band is a cliff, not an allowance: `Q = N − 1` bills 0 and `Q = N + 1` bills the full rate on everything; D-59). On an untiered `per_unit` usage row the compile **rewrites** `modelKind` to `graduated` (the authored unit price becomes the `[N, null)` band rate) — band rows are structurally forbidden on `per_unit`, so the rewrite is what makes the compile legal; the authored kind is retained beside the marker. Per-seat-scaled allowances are a named Future decision gate.

**Rationale**: Existing SKUs carry "N included, then rate" semantics; the $0-band covered the math but not display/reporting, and rollover was inexpressible. Compilation keeps both executors unchanged — bands for the math, grants for the cross-period state — first-class authoring with zero new evaluation machinery. Allowance netting at the usage source is prohibited by the same doctrine as D-44 (the commercial rule lives in the catalog; sources emit raw usage).

**Actors**: `cpt-cf-bss-pricing-actor-catalog-admin`, `cpt-cf-bss-pricing-actor-billing`

#### Prepaid credit grant

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-prepaid-credit-grant`

A plan **MAY** declare a prepaid credit grant (`grantAmount` > 0, `creditUnit` = currency or **published** `meteringUnit`, `category` = `prepaid` (default)|`promotional`, `price` per `(currency, region)` (`category = prepaid` only), `expiryPolicy` = `never`|`days(N>0)`, `autoRechargeAllowed` (`prepaid` only), `applicability` = `all_usage` (default)|set of published usage meters, `drawdownPriority` = optional int ≥ 0), frozen in `pricingSnapshotRef` (§17.7). Publish **MUST** fail on an unset `expiryPolicy`, an unpublished `creditUnit`, an unscoped grant price for a multi-`(currency, region)` plan, a `promotional` grant carrying price rows or `autoRechargeAllowed`, or an `applicability` entry that is unpublished or not a usage line of the plan (a metered `creditUnit` additionally bounds `applicability` to that unit's meters); publish **MUST materialize** the resolved `applicability` into the snapshot — the executor never infers scope (D-43); `days(N)` anchors at grant issuance (the purchase or recharge instant, UTC); grant-price changes evaluate as ordinary per-currency price deltas under the material-change policy, while `category`, `applicability`, and `drawdownPriority` changes are **always material** (no numeric delta — the G1 fail-safe). `drawdownPriority` is an authored **default**: the effective order across the grants an account holds is **Billing-owned**, resolved deterministically as `drawdownPriority` → `promotional` before `prepaid` → earlier expiry → earlier issuance → `grantId` (D-43). The catalog **MUST NOT** persist any balance, compute drawdown, or order live balances.

**Rationale**: Wallet-style selling needs a defined grant primitive with balance execution owned by Billing/Rating.

**Actors**: `cpt-cf-bss-pricing-actor-billing`

#### Derived (composite) meter

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-derived-composite-meter`

A price row **MAY** use a derived (composite) meter; publish **MUST** persist >= 2 **published** constituent `meteringUnit` ids, the formula expression **as data**, and a single **output unit**, and **MUST** fail if any constituent unit is unpublished or the formula self-references. The definition **MUST** be frozen in `pricingSnapshotRef`; Tariffs evaluates the formula and the catalog **MUST NOT** compute the result.

**Rationale**: Multi-unit offerings (e.g. VM = vCPU + RAM) need a composable, frozen definition without computing math here.

**Actors**: `cpt-cf-bss-pricing-actor-rating`

#### Interim discount reference hook

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-discount-ref-hook`

An optional `discountRef` **MUST** be validated for referential integrity (resolves to a registered external discount instrument) and persisted on the snapshot; the catalog **MUST NOT** author, evaluate, or stack the discount, and its absence **MUST NOT** block publish. Evaluation is owned by Tariffs/Promotions.

**Rationale**: A day-1 discount hook must be sellable without the catalog owning discount authoring/evaluation.

**Actors**: `cpt-cf-bss-pricing-actor-promotions`

#### Minimum-quantity floor typing

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-min-qty-floor`

A `minQtyThreshold` **MUST** declare its floor type — `purchase` (Subscriptions rejects orders below the floor) or `usage` (Tariffs/Rating treats usage below the floor as ineligible and fails closed, never silently zero-rated). A `usage` floor **MUST** additionally declare its fallback on the row; at launch the only supported fallback is **`exception`** (the below-floor usage line fails closed into the rating exception path — visible and resolvable; richer fallbacks are Future scope). Publish **MUST** reject a `minQtyThreshold` with no declared floor type and **MUST** warn if it falls inside a non-zero-priced band. The type + value + fallback are frozen in `pricingSnapshotRef`.

**Rationale**: An untyped floor is ambiguous between order-time rejection and downstream eligibility.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Trailing-tier qualification

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-trailing-tier-qualification`

A tiered `usage` price row **MAY** set `tierQualificationWindow` (`current` (default) | `trailing_period`) — a third window, distinct from `tierAggregationWindow` and `billingGranularity` (D-40). `trailing_period` qualifies the rate tier from the **prior anchor-derived billing period's total** (never a calendar month; single-band volume-style selection), **locks that one rate for the whole current period**, and bills actual usage at `billingGranularity`; `current` preserves existing tiered behaviour exactly. The lock is **per subscription per billing period**, so it does **not** live in `pricingSnapshotRef` (plan-scoped, publish-time, immutable — one published row serves N subscriptions with N anchors and N trailing totals): the catalog freezes only the authored window and band set, and **Rating** pins `{priceId, periodStart, qualifiedBandId, lockedUnitPriceMinor}` on the period's rated charges beside `pricingSnapshotRef` — that pin is the replay source (D-60). First-period **bootstrap** (no trailing history) **MUST** resolve to the **lowest tier** and is pinned the same way. The trailing total is a **separate** aggregate over the prior anchor period, independent of `tierAggregationWindow`; `trailing_period` with `tierAggregationWindow ∈ {subscription_lifetime, per_event}` **MUST** fail publish (`TIER_QUAL_WINDOW_INCOMPATIBLE`), as **MUST** `trailing_period` on a row whose lowest band is `$0` (`TIER_QUAL_ZERO_BAND_LOCK` — single-band selection would lock the free rate for a whole period). Publish of any `trailing_period` row without the green `trailing_tier` joint fixture is blocked (`FIXTURE_MISSING`). An **operator-authored** bootstrap tier is **not** authorable at launch (2026-07-28 review fix): it would need its own field, a publish check binding the value to one of the row's bands, and a snapshot segment — none of which exist; it is a named Future gate. An **explicit** `tierQualificationWindow` of **any** value — `trailing_period` **or** `current` — on a non-tiered (`flat`/`per_unit`/`package`) or non-usage row **MUST** fail publish (`TIER_QUAL_ON_NON_TIERED`, 422): the field is meaningless there and an accepted-but-ignored value would mask authoring errors (fail-closed; 2026-07-28 review fix, widened from `trailing_period`-only; D-68). The catalog authors the window and **MUST NOT** compute the qualification or the trailing aggregate — Rating supplies the trailing total and re-qualifies at each period boundary; Tariffs applies the locked rate.

**Rationale**: Prior-period-qualifies / current-period-billed pricing (canonical: PaaS egress) is inexpressible through the aggregation window alone; the authored commercial choice must live in the catalog and freeze in the snapshot (D-40).

**Actors**: `cpt-cf-bss-pricing-actor-rating`

### 6.11 Operator Efficiency

#### Plan clone

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-plan-clone`

Clone **MUST** create a new `planId` in `draft` with copied configuration and **new** price ids, record `clonedFrom`, and **MUST NOT** affect source subscriptions. `PriceWindow` schedules are runtime state, **not** configuration — they are never cloned, and the clone's billable rows cannot publish until fresh windows are scheduled (window-coverage validation applies normally). `priceEligibility` and `grandfatherUntil` **MUST** reset to defaults (not copied), contract locks **MUST NOT** be copied, and `discountRef` **MUST** be copied only if it still resolves to a registered instrument (else dropped with an operator notice). `existing_grandfathered` rows are lifecycle state, not configuration — they are **not** cloned (the `all_subscriptions` successor row carries the going-forward price); superseded/closed historical rows are likewise not copied.

**Rationale**: Cloning accelerates authoring without carrying eligibility/lock state that must be re-decided.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Bulk price import

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-bulk-price-import`

Bulk price import operates on the **draft plane only** (**D-118**, 2026-07-31 review fix — flagged for veto): rows land as new scope keys or edits of existing **draft** rows; a row addressing a **published** row's scope key whatever its content (**D-287**, 2026-08-09 — the "with changed content" qualifier is removed; flagged for veto) **MUST** fail validation per-row (`IMPORT_TARGETS_PUBLISHED`) with the repricing run named as the remediation — published rows are append-only and change only through the D-88 supersession units with a bounded changeover instant, which the import deliberately does not carry (one bulk mechanism for published prices). It **MUST** validate all-or-nothing **pre-commit** (any invalid row blocks the whole batch with a per-row report); at **commit** it **MUST** take optimistic per-row locks and a per-row ETag conflict **MUST** fail only that row (batch reported partial; committed rows stand; conflicted rows listed for retry) — never silently overwrite. A material batch's approval pins **per-row content**; the committed subset **MUST** be a subset of the approved set, and a retry of conflicted rows with unchanged content reuses the original approval (a changed row starts a fresh approval).

**Rationale**: Annual repricing must be efficient yet safe against concurrent manual edits.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Mass repricing

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-mass-repricing`

A mass price adjustment over N rows **MUST** be idempotent (re-run-safe after partial failure) and emit deduplicated events; it **MAY** coalesce into one or few `CatalogVersion`s. Repricing selectors **MUST** exclude `existing_grandfathered` rows (immutable in price); an explicit inclusion fails that row with a per-row error — never a silent skip, never a reprice. The throughput SLO **MUST** be back-calculated from the tenant worst-case row count against an agreed maintenance window (>= 50 rows/sec — ratified 2026-07-28 as the launch default; the perf test MUST verify it against the tenant worst-case row count, an engineering validation, not a re-decision).

**Rationale**: Large repricing runs must be safe to retry and must not flood consumers with duplicate events.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Mutation idempotency keys

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-mutation-idempotency`

A plan/price create/update call carrying a client idempotency key **MUST** return the original result on retry (no duplicate draft plan or price row) **once the original has completed**, for a documented TTL; a key reused after the TTL **MAY** be a new request. An idempotency-key replay during an active bulk lock **MUST** return the original completed result regardless of current lock state.

That qualifier narrows this **MUST** deliberately (**D-143**, 2026-08-02 implementation finding — **flagged for veto**): a duplicate can arrive while the original is still in flight with **no contract violation by either caller**, and there is then no original outcome to return. The gear **MUST** answer that case `IDEMPOTENCY_KEY_IN_FLIGHT` (409) — *this key is claimed and not yet answered; retry, and you will receive either the stored response or `IDEMPOTENCY_PAYLOAD_MISMATCH`* — never a not-found refusal on a request that both exists and is succeeding, and never a request held open for the duration of an unrelated caller's transaction. The rejected alternative (block-and-replay, which keeps this **MUST** unqualified) is recorded with the decision.

The TTL is a **bound**, not a label (**D-142**, 2026-08-02 implementation finding): expiry **MUST** be evaluated when the key is claimed, and **before** the retried payload is compared against the stored request digest. Comparing the digest first makes the TTL unreachable — the first payload to touch a key would own it for good — so the ratified 24-hour value (AC #100) would bound nothing, and "a key reused after the TTL MAY be a new request" would never come true for any caller. An answer, once recorded, is **written once**: a retry after completion returns the **stored** response and never re-executes or overwrites it, which is exactly what this requirement promises a retry.

The key travels in a **required `Idempotency-Key` request header** (**D-171**, 2026-08-03 implementation finding — the design set named the concept on every API table and the header nowhere), and what is compared against the stored digest is the request **as this gear models it**, not the bytes it arrived in (**D-174**, same finding). "A request-payload digest" left the subject of the hash unstated, and the two readings differ for a caller: over the received bytes, a retry that merely re-serializes the same request — a different client, a re-encoding proxy, any library whose member order is not stable — is refused `IDEMPOTENCY_PAYLOAD_MISMATCH`, which is a 409 the caller cannot fix on the one call this requirement obliges them to retry. The digest is therefore over a canonical rendering of the **parsed** request, so a mismatch means the caller changed the request and not that the encoder did; the consequence, stated rather than discovered, is that a retry adding a member this gear does not model **replays**, which is the correct answer for a field the gear ignores and the same fact as not refusing a legitimate re-encoding.

**Rationale**: Safe retries require idempotent mutation semantics with a defined replay window.

**Actors**: `cpt-cf-bss-pricing-actor-finance-manager`

#### Price history and export

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-fr-price-history-export`

The system **MUST** return chronological immutable price-history records with actor and effective dates under Auditor/Finance filters and **MUST** support export within p95 <= 5s **per 100-record chunk**, the budget scaling linearly with the requested page size up to the D-125 hard cap — so a full 1,000-row page is an export/stream shape, never an interactive read, and interactive collection reads keep the 100-record server default (2026-08-01 review fix, C-6: D-125 raised the cap to 1,000 while the SLO's unit stayed 100, which read literally put a full page at 50s). History and audit list reads **MUST** paginate (an opaque cursor + a bounded `limit`, stable commit order — no skipped or duplicated rows across a page walk concurrent with writes; D-125): the surfaces read >= 7 years of append-only rows.

**Rationale**: Auditors and Finance must reconcile contract and billing disputes from complete immutable history.

**Actors**: `cpt-cf-bss-pricing-actor-auditor`

### 6.12 Access Control and Governance

#### RBAC deny-by-default

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-rbac-deny-by-default`

Plan mutate APIs **MUST** be deny-by-default: a mutation is permitted only to the roles the role matrix grants the relevant `(resource, action)` (CatalogAdmin/ProductManager/FinanceManager for authoring mutations; FinanceReviewer for approval decisions and threshold policy; Auditor is read-only), and denied attempts **MUST** be audit-logged; **read/preview** APIs **MUST** also be deny-by-default (an unlisted-role principal is denied unless it holds an explicit **catalog-preview read grant**, region/brand-scoped by IdP claims). ~~The historical-import (backdating) capability **MUST** be a distinct restricted grant, not a default role.~~ — **struck by D-330** (2026-08-16): historical import is out of scope, so no such capability is issued and none is targetable.

**Rationale**: Pricing data is commercially sensitive; both mutation and preview must be least-privilege.

**Actors**: `cpt-cf-bss-pricing-actor-catalog-admin`

#### Tenant and brand isolation

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-tenant-brand-isolation`

The system **MUST** enforce tenant isolation and scope brand/region per IdP claims at the gateway (authorization/display scope). Pricing `region` is **decoupled** from the IdP authorization region: authoring or mutating a price row scoped to a pricing `region` the user's authz scope does not grant **MUST** be denied and audit-logged.

**Rationale**: Cross-tenant leakage is a critical incident class; authz region must not be conflated with pricing region.

**Actors**: `cpt-cf-bss-pricing-actor-catalog-admin`

#### ~~Historical-import (backdating) governance~~ — struck by D-330

**Struck 2026-08-16 by D-330.** The requirement `fr-historical-import-governance` **leaves the
requirement set**: historical import is out of scope, this gear serves the prices it authored, and
a subscriber whose price predates the catalog is re-papered onto a plan the catalog publishes. It
is therefore claimed by **no** design slice — Slice 5's Traces-to line drops it in the same wave —
and that is the correct end state rather than a coverage gap. **A struck requirement is not an
unbuilt one**: nothing here is owed, no slice is short of an obligation, and the total requirement
count falls by one. Its `**ID**:` declaration is removed rather than annotated, because a
requirement that still declares an id is still a requirement, and the every-requirement-owned rule
would then be asking a slice to own a scope decision's remains. The text it stated is kept below,
struck, so the record says what went and by whose decision.

**It is named by its bare `fr-` slug here and in AC #65, not by the full gear-prefixed
id**, and that is the same rule one step further: an id with no definition
anywhere in the artifact set is a dangling reference, which `cfs validate` reports as an error
(`ref-no-definition`) against whichever line spells it. The slug identifies the struck requirement
exactly and reads as prose rather than as a citation. Do not restore the prefix — the record does
not need it, and the gate reads it as a claim that the definition still exists.

The rules it carried go with it: the restricted grant and mandatory reason, the row-shape parity
subset, the disjoint reference store (**D-76**), the always-material second person (**D-13**), the
temporal bound on the reference interval (**D-81**), and the field-complete tiered reference row
(**D-87** (i)). **D-87's other half survives** and is load-bearing — a `migrated-origin` payload
must carry the complete evaluable row content because no `CatalogVersion` backs it — and lives on
in `cpt-cf-bss-pricing-fr-migration-safety` and AC #58, where it always belonged.

~~The historical-import path that permits past `availableFrom`/effective dates **MUST** require a **restricted role** and a **mandatory reason**, **MUST** validate every imported row against the same row-shape rules as regular authoring (taxonomies, precision, model shape **including tier-band geometry over the reference row's own band set and the package fields — a tiered legacy price is importable field-complete, D-87**, and scope-key + effective-interval uniqueness **within the reference store** — an import can never create a row authoring would reject), **MUST** land those rows in a store **disjoint from published price rows** (D-76 — so a reference price for a still-published plan is not rejected as a duplicate of its live row, and no reference row can enter coverage, sellability, the read model or a `CatalogVersion` through a missed filter), **MUST** be an **always-material change** (independent second approver required — backdated rows shape `migrated-origin` snapshots that rating consumes going forward), **MUST** be fully audited, and **MUST NOT** generate or re-open any downstream billable charge window. A reference row's `effective_from` **MUST** be strictly past, while its `effective_to` **MAY** reach `now` or be open-ended (D-81 — the legacy price book's standing price is importable as still-in-effect, which is what makes synthesis tier 2 reachable for the per-trigger instant; the disjoint store keeps such a row structurally inert everywhere except synthesis). It is the **only** sanctioned backdated reference path (e.g. for legacy snapshot synthesis).~~

#### Audit completeness and tamper-evidence

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-fr-audit-completeness`

Any plan/price mutation **MUST** record actor (as a **pseudonymous principal id** — no display names/emails, keeping operator PII out of the long-retention store), timestamp, before/after version, and approval trail; price history **MUST** be retained **>= 7 years** (tenant/jurisdiction-configurable; the **maximum** applicable minimum where several apply) and stored **append-only / tamper-evident** (hash-chained in the transactional store — D-14; optionally anchored to external WORM) so prior versions cannot be mutated or deleted within the retention window. Each record also carries the **correlation of the request that caused it** — one value per operator call, shared with every event that call emits (**D-178**, 2026-08-03 implementation finding): the design set required the field on both stores and named no producer for either, so the join an auditor needs to read one operator action across two audit chains rested on nothing. It is the request-scoped value the gear's HTTP edge establishes, minted there when nothing is propagated inbound, and it is never the client idempotency key; it is also **minted rather than adopted from an inbound trace id** (**D-181**, 2026-08-03), because a distributed trace can span hundreds of operator calls and this field's whole job is to name one. **Every** mutating authoring surface writes a record, not only the publish (**D-175**, same day): the `action` vocabulary's opening enumeration named one of the six and the section's own MUST obliged all of them, so the trail was silently incomplete rather than visibly absent. The same obligation reaches the **approval plane** (**D-180**, 2026-08-03): opening a change unit and withdrawing a pending one are each a mutation with an actor, so each writes its own record — while the machine-driven TOCTOU void writes none, its cause having already written one under the actor who caused it.

**Actors**: `cpt-cf-bss-pricing-actor-auditor`

## 7. Non-Functional Requirements

### 7.1 NFR Inclusions

> The targets below (availability/DR, mass-repricing throughput, size caps, idempotency-key TTL) were ratified 2026-07-28 as committed launch defaults (§15); the mass-repricing figure is additionally perf-test-verified against the tenant worst case.

#### Plan mutation latency

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-nfr-mutation-latency`

Plan create or update (end-to-end persistence and validation) **MUST** complete within **2 seconds at p95**.

**Threshold**: p95 <= 2s for plan create/update.

**Rationale**: Operator authoring throughput depends on responsive mutation.

#### Price field validation latency

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-nfr-validation-latency`

A single price row save (currency/region/tier validation) **MUST** complete within **200ms at p95**.

**Threshold**: p95 <= 200ms per price-row validation.

**Rationale**: Inline validation must feel immediate in the tier/price editor.

#### Publish propagation

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-nfr-publish-propagation`

Plan definitions **MUST** be visible to Rating cache warming within **5 seconds at p95** of `CatalogVersionPublished`; the registry **MUST** bound the delay from `PlanPublished` (pending) to `CatalogVersionPublished` (committed) by the **max batching-delay SLO** (D-47: interactive <= 5s coalescing; bulk <= 5 min hard max; p95 pending→committed <= 60s).

**Threshold**: p95 <= 5s from `CatalogVersionPublished` to read-model visibility; batching delay p95 <= 60s / max 5 min (D-47, ratified 2026-07-28).

**Rationale**: Rating and portal depend on timely propagation of committed catalog content.

#### Plan read latency

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-nfr-read-latency`

Catalog browse or plan-detail read served from the read model within a tenant partition **MUST** have p95 latency **under 100ms**, holding at the 20-currency floor.

**Threshold**: p95 < 100ms per tenant partition (at >= 20 currencies/plan).

**Rationale**: Rating and portal reads are on the hot path.

#### Event propagation

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-nfr-event-propagation`

`PlanCreated`/`PlanUpdated` (and the other producer events) **MUST** reach downstream consumers within **5 seconds at p95**, carry correlation/idempotency keys, and be safe to redeliver (at-least-once; consumers dedupe by idempotency key).

**Threshold**: p95 <= 5s to consumers; duplicate redelivery safe.

**Rationale**: Downstream cache warming and eligibility depend on timely, dedupable events.

#### Multi-currency scale

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-nfr-multi-currency-scale`

The system **MUST** support **at least 20 currencies per plan as a guaranteed floor** (tested at 20), and the read SLO (p95 < 100ms) **MUST** hold at that floor, not only at nominal load.

**Threshold**: >= 20 currencies/plan with read p95 < 100ms sustained.

**Rationale**: Global plans carry many per-market rows; the read path must not degrade.

#### Mass-repricing performance and idempotency

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-nfr-mass-repricing-throughput`

A mass price adjustment over N rows **MUST** be idempotent (re-run-safe) and emit deduplicated events; the throughput SLO **MUST** be back-calculated from the tenant worst-case row count (plans x currencies x regions) against an agreed maintenance window.

**Threshold**: >= 50 rows/sec — ratified 2026-07-28 as the launch default; the perf test verifies it against the tenant worst-case row count (an engineering validation, not a re-decision).

**Rationale**: Annual/bulk repricing must complete within a maintenance window and be safe to retry.

#### Read-model availability and disaster recovery

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-nfr-availability-dr`

The published read model **MUST** meet a stated availability SLO and disaster-recovery objective; during a read-model outage, charge resolution **MUST NOT** resolve from an unavailable or partially-warmed read model and **MUST** fail closed (frozen `pricingSnapshotRef` resolution is unaffected).

**Threshold**: Availability >= 99.9% per tenant partition; RPO <= 5 min; RTO <= 30 min — ratified 2026-07-28 as launch targets.

**Rationale**: Rating has a hard real-time dependency on the read model; best-guess pricing under lag is a show-stopper.

#### Audit retention and tamper-evidence

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-nfr-audit-retention`

Audit and price-history records **MUST** be retained **>= 7 years** (tenant/jurisdiction-configurable; the **maximum** applicable minimum where several apply) and stored **append-only / tamper-evident** (hash-chained in the transactional store; optionally anchored to external WORM) so prior versions cannot be mutated or deleted within the retention window.

**Threshold**: >= 7-year retention (configurable); 100% append-only/tamper-evident storage.

**Rationale**: Financial audit and dispute resolution require immutable, long-lived history.

#### Data residency for price and audit data

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-nfr-data-residency`

Price and audit/history data for a residency-bound tenant **MUST** stay within the configured jurisdiction's residency boundary and **MUST NOT** replicate a row outside **any** of its residency jurisdictions' boundaries, consistent with the jurisdiction-configurable retention.

**Threshold**: Zero rows replicated outside a residency jurisdiction boundary.

**Rationale**: Data-residency violations are compliance incidents.

#### Plan and tier size limits

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-nfr-size-limits`

The system **SHOULD** enforce configurable soft caps on tier-band count per row and price-row count per plan, emitting a publish warning above the cap, to protect the read SLO and mass-repricing SLO from pathological plans. The custom-interval cap referenced by `fr-custom-frequency` (AC #84) belongs to the same limit set. **The warning is a coded advisory, `PLAN_SIZE_SOFT_CAP_EXCEEDED` (Foundation §3.3, D-160, 2026-08-03, found while building the publish rule set)** — carried in the validation report's advisory `warnings[]` channel, naming the cap, its limit and the offending count, and **never blocking**, which is what the `SHOULD` above means. Until D-160 no document named an advisory code at all, so this requirement had nothing to be reported through and was therefore unbuildable rather than merely unbuilt. One code covers both caps: the operator's action has the same shape either way — shorten the ladder, or split the plan — and the report says which cap was passed. The two **interval** caps are a different kind and are not part of this rule: they are hard, and a custom frequency beyond them fails publish (`INVALID_CUSTOM_INTERVAL`).

**Threshold**: <= 100 tier bands/row, <= 500 price rows/plan (soft, tenant-configurable); custom-interval cap `customEveryNDays` <= 366, `customEveryNMonths` <= 24 — ratified 2026-07-28 as launch defaults. All four are **declared per tenant in `pricing_policy_object`**, the store this gear already uses for its approval-threshold, tax-display and default-rounding policies (**D-152**, 2026-08-03): "tenant-configurable" had named no carrier, and the gear's configuration section is per **deployment**, so every tenant of a deployment shared one cap. A tenant with no entry takes the ratified default above.

**Rationale**: Unbounded plan size degrades the read and repricing hot paths.

#### Observability and alerting

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-nfr-observability`

Every alarm a design slice declares (each slice's §7) **MUST** be routed to an owned, acknowledgeable production channel; every **Critical** alarm (`pricing.readmodel.pin_eligibility_overdue`, `pricing.window.changeover_unwarmed`, `pricing.catalogversion.commit_overdue`, `pricing.audit.chain_gap`, `pricing.contracts.enum_drift`) **MUST** carry a runbook naming the remediation its declaring slice states. Slice-declared metrics and alarms are **normative surface**, not implementation suggestions — the slices hang money-facing failure modes (replay divergence, selling into an unwarmed changeover, audit-chain gaps) on them, and an unrouted alarm is a silent failure (2026-07-31d billing-domain review, C-5: the design set declared ~two dozen alarms while the PRD contained no observability requirement obliging any of them to be routed, acknowledged, or given a runbook).

**Threshold**: 100% of declared Critical alarms routed + runbook-linked before GA; alarm-to-acknowledgement tracked for Criticals.

**Rationale**: This gear's availability and RTO targets are already committed (§7.1); the alarms are how those targets and the money-facing invariants are operable.

### 7.2 NFR Exclusions

Explicit dispositions for domains not owned by this PRD (no silent omissions):

- **Tax computation / scheme-determination performance**: Not applicable — Tax Engine / Billing; the catalog persists only the display basis and `taxCategory` reference.
- **Proration / plan-change math performance**: Not applicable — the rating gear evaluates proration (Subscriptions owns the boundary/mode); this PRD publishes the inputs.
- **FX / overlay evaluation performance**: Not applicable — Tariffs owns FX and overlay stacking; the catalog performs no FX or evaluation.
- **Prepaid balance-ledger / drawdown performance**: Not applicable — Billing / Rating; the catalog defines the grant only, never tracks balance.
- **Frontend UX performance / accessibility (WCAG) / i18n rendering**: Not applicable to this backend PRD — owned by the corresponding frontend DESIGN (Presentation renders localization).

## 8. Five Quality Vectors Analysis

| **Quality Vector** | **Show-Stopper Requirements** | **Rationale** |
|--------------------|-------------------------------|---------------|
| **Efficiency** | Self-service plan/tier config; clone; bulk import; >= 90% reduction in manual IT for standard plans | Faster commercial packaging for partners and product lines |
| **Reliability** | Fail-closed publish; consumer-side read-model determinism + monotonicity per `CatalogVersion`; read-model availability/DR for Rating; cross-PRD conformance fixtures (tier-boundary, proration); fan-out atomicity (retry-to-SLO or `PlanPublishDegraded`); immutable price history; deterministic ids on all charges; migration without invoice mutation | Billing incidents from bad config, partial publish, read-model outage, or cross-PRD seam drift are show-stoppers |
| **Performance** | Read p95 under 100ms; publish fan-out under 5s; validation under 200ms for price save | Rating and portal depend on the catalog read path |
| **Security** | Two-person rule with segregation of duties (no self-approval); RBAC (the scoped historical-import capability is struck — D-330); tenant/brand/region isolation; audit on deny; append-only/tamper-evident audit and price history; per-jurisdiction data residency | Unauthorized or self-approved pricing change is a financial-fraud risk; tamper-evidence and residency are compliance gates |
| **Versatility** | One-time, recurring (monthly to custom cycles), usage, hybrid, per-seat; add-ons; bundles; 20+ currencies; phases; grandfathering; migration | Required for IaaS/PaaS/SaaS and marketplace |

**Note**: Plan retirement and migration contributes to **Reliability** and **Versatility**. Billing descriptors contribute to **Reliability** (invoice reproducibility).

## 9. Public Library Interfaces

> Plan & Price Modeling is a backend catalog capability, not a client library. Interfaces below are high-level contracts; concrete API schemas, endpoints, and DDL belong in DESIGN.

Every REST surface of this gear — the contracts below and every slice endpoint — is served
under the gear's service prefix `/bss-pricing/v1/{resource}`, and expresses an action on a
resource as a sub-resource segment rather than a colon-suffixed custom method (D-140). The
prefix is the platform route convention and is machine-enforced; the paths named throughout
this PRD and the design set are the wire paths.

### 9.1 Public API Surface

#### Plan/Price authoring and publish contract

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-interface-authoring-publish`

**Type**: conceptual authoring + publish contract (shape in Design)

**Stability**: stable (contract intent), schema unstable (Design owns)

**Description**: Create/update/clone plans and price rows in `draft`, run fail-closed publish validation, submit for approval (two-person rule), and publish — emitting the frozen event set and requesting a `CatalogVersion` increment. Accepts client idempotency keys; enforces optimistic concurrency (ETag).

**Breaking Change Policy**: Major version bump for incompatible request/response changes; the publish-validation and event-name contracts are part of the contract.

#### Published catalog read model

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-interface-catalog-read-model`

**Type**: conceptual read-model contract (shape in Design)

**Stability**: stable (contract intent), schema unstable (Design owns)

**Description**: Exposes, per committed `CatalogVersion`, the complete plan/price read model — `{skuId, planId, priceId}`, model kind, ordered tier bands, evaluation-policy fields, phase->price map, billing descriptors, proration/plan-change/entitlement contracts, reserved-rate and derived-meter definitions — resolvable via `pricingSnapshotRef`, monotonic per version, no draft reads.

**Breaking Change Policy**: Additive fields only within a major version; removing or redefining a published field is a breaking change.

#### Effective catalog price preview

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-interface-price-preview`

**Type**: conceptual preview contract (shape in Design)

**Stability**: stable (contract intent), schema unstable (Design owns)

**Description**: Returns the catalog **base** list price, `taxInclusive` flag, tier summary, and trial length (`displayTrialDays`) for a `(region, currency)`; fails closed if no row exists (no implicit FX); indicates that Contract/`PriceOverlays` apply at purchase (Tariffs). Requires a catalog-preview read grant.

**Breaking Change Policy**: Additive; the base-price-only semantics (no overlay evaluation) are part of the contract.

### 9.2 External Integration Contracts

#### Tariffs read-model contract

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-contract-tariffs-readmodel`

**Direction**: provided by this PRD to Tariffs

**Protocol/Format**: model kind, ordered tier bands, `tierAggregationWindow`, `billingGranularity`, `priceEligibility`/`grandfatherUntil`, per-key `PriceWindow` **intervals** + coverage end (D-99 — intervals, never a point-in-time flag, so "active at `t`" is derived at read time; the projected row/window set spans the D-121 horizon — every row whose windows intersect `[projection − H, ∞)`, `H` = 2 × the longest cycle sold, states `scheduled | active | expired` — so arrears resolution at a recent past `t` under a current pin always finds its window, and older re-rates replay from their originally-pinned versions), the plan-change inputs incl. `usageCounterOnPlanChange` (D-113), reserved-rate/flavor, derived-meter definition, `customerGroup` `PriceOverlay` + resolved membership; all frozen in `pricingSnapshotRef` (Design). **Second lane — the `migrated-origin` snapshot (D-102, 2026-07-31 review fix):** a `migrated-origin` ref resolves through **no** `CatalogVersion` by construction (D-76/D-87), so it cannot be served off the read model; Rating/Tariffs fetch its self-contained payload + provenance from `GET /bss-pricing/v1/migrated-origin-snapshots/{subscriptionRef}` under the service identity's `plan × read` (Design 11 `inst-sy-surface`). The payload carries the resolved rows' full evaluable content **plus the plan-level billing descriptor set and resolved grant set** (2026-08-01 review fix, C-5): a `migrated-origin` ref pins no `CatalogVersion` by construction (C-5's second leg, a fully-legacy tier-2 key with no plan revision, is struck with tier 2 by D-330 — the first leg carries the rule alone), so a row-only payload left Billing unable to post the line it could rate. Until this lane the payload D-87 requires them to evaluate from had no reader-facing surface anywhere in the set.

**Compatibility**: No default substitution; absence of a required field must have failed publish; Tariffs never reads draft state; a consumer pins only a **pin-eligible** `CatalogVersion` (D-101 + D-114 — committed, every subject row of that version warm-complete, *and* every earlier version itself pin-eligible: the prefix closure is what makes "one pin never resolves two different contents over time" hold, since per-subject resolution reaches through the pin into older deltas), **obtained from the published pin frontier** — `GET /bss-pricing/v1/catalog-version/frontier` (**D-136**, 2026-08-01 review fix: pin-eligibility is a version-level, prefix-closed, recursive predicate no consumer can evaluate for itself, it was stored nowhere and exposed nowhere, and the `pricing.readmodel.pin_eligibility_overdue` alarm had nothing to read; it is now a materialized per-tenant watermark advanced by the projector, walked forward through every already-complete following version of that tenant and scoped to that tenant's own committed versions rather than to the cross-tenant sequence — **D-164**, 2026-08-03). **What a version can answer is bounded by the slices that have landed (D-167):** before Slice 7's `PriceWindow` store, Slice 4's and Slice 10's gate flags and the registry `sellable` flag, a plan subject carries none of those facts, so three of the six sellability predicates are not evaluable from it — Tariffs reads the pin frontier and the delta as the finished contract describes them, and this note is what says which parts of that contract a build has reached.

#### Subscriptions publish contract

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-contract-subscriptions`

**Direction**: provided by this PRD to Subscriptions — **two-way**: **three** inbound lanes — (1) grandfathering re-bind reporting (recorded 2026-07-28, previously implicit), (2) the **migration execution handshake** (D-65, recorded 2026-07-29: execution-start declaration + processed-scope report), and (3) **per-scope-key in-flight-subscription presence** (D-79, recorded 2026-07-30: the data source of the D-51/D-62 predicates and the retirement story's subscription count)

**Protocol/Format**: eligible plans; migration targets + `PlanMigrationScheduled`; phase map + `convertsToPhaseId` + `displayTrialDays`; proration input contract (`billingAnchorPolicy`, `prorationBasis`, `creditOnDowngrade`); plan-change contract (`allowedChangeTargets`, `comparabilityRank`); entitlement grant set; sellability gate (all six predicates: active window **with the D-80 coverage horizon** — coverage through `now + the longest billing cycle sold on the key` — committed version, availability dates, lifecycle state, GA flags, registry `sellable` flag — standalone lines only, D-46; evaluated as the **conjunction over every key the purchase binds** on the bound market — components + phase chain, D-94); grandfathering eligibility surface (`priceEligibility`/`cohort`/`grandfatherUntil` + the read-time `EligibilityExpirySignal`) with the **inbound re-bind feedback**: at each renewal of a subscription bound to an expired generation, Subscriptions reports the outcome — the released generation on re-bind, still-bound otherwise — the input the `pricing.grandfather.expiry_signal_backlog` alarm consumes (Design 07). **Migration execution handshake (inbound lane 2, Design 11 §5):** before its first `PlanLink` batch Subscriptions **MUST** call `POST /bss-pricing/v1/migrations/{id}/start`, which flips `scheduled → in_progress`, runs the D-36 execution-time re-validation of contract locks and boundary deltas, and returns the **exclusion set** Subscriptions MUST honour; Subscriptions re-reads the schedule state per processing batch thereafter (so a D-34 in-flight cancel is observed), honours the `(migration_id, subscription)` dedup key (M2), and closes with `POST /bss-pricing/v1/migrations/{id}/complete` carrying the processed/excluded/failed sets. Without this lane the `scheduled → in_progress → completed` transitions have no trigger and D-34/D-36/D-38 rest on an unspecified handshake (Design). **In-flight-subscription presence (inbound lane 3, D-79 — Designs 07/11):** on request, Subscriptions reports, for a submitted price-id set, a **per-price-id presence map** over **non-terminal** subscriptions whose pinned `pricingSnapshotRef` references each id (**D-131**, 2026-08-01 review fix — the lane previously returned one aggregate **count over the whole set**, while every consumer decides **per canonical scope key**: a union count answers only "does this plan have any subscriber at all", under which retirement would cancel nothing whenever one key were occupied, and the per-key alternative meant N synchronous cross-gear round trips inside the mutating ACID transaction. A mutating unit now makes **one** call over the union of its touched keys' price-id sets, derives per-key presence from the map, and carries a stated timeout whose expiry is the fail-closed case) — the input of the D-51 retirement predicate ("no in-flight subscribers on the key"), the D-62 cancel/shorten exemption (narrowed by D-80 to keys that are also not currently sellable — "sellable" per the D-94 plan-market conjunction, so a component key of a sellable plan-market is never exempt), the always-material routing, and the retirement story's "view the active subscription count" (the plan-level total is the same call over all the plan's price ids). Pricing re-resolves the predicate **inside the mutating commit** and **fails closed on lane outage** — subscribers presumed present: windows kept, gap check + approval apply. Without this lane those predicates have no data source (the D-01 defect class).

**Compatibility**: Catalog publishes inputs; Subscriptions owns the change boundary/runtime/`PlanLink`; the rating gear evaluates proration; cross-currency/region/frequency mid-cycle change is unsupported at launch (cancel + new); the re-bind feedback field shape closes at design freeze with Subscriptions (their seam SUB-P6).

#### Registry CatalogVersion increment contract

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-contract-registry-catalogversion`

**Direction**: bidirectional with the catalog registry

**Protocol/Format**: on `PlanPublished` the catalog requests addressability; the registry (sole incrementer) MAY batch approved publishes into one discretionary catalog publish and emits `CatalogVersionPublished`; `pricingSnapshotRef` pins the committed version (Design).

**Compatibility**: Pending-ref on `PlanPublished`; committed version pinned in the snapshot; increment-trigger taxonomy + max batching-delay SLO ratified 2026-07-28 (D-47: interactive <= 5s coalescing; bulk <= 5 min hard max; p95 pending→committed <= 60s) — mirrored in the products-gear PRD (D-47).

#### Billing descriptor contract

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-contract-billing-descriptors`

**Direction**: provided by this PRD to Billing

**Protocol/Format**: billing descriptor set (invoice line template, `glCode`, itemization rule) frozen into `CatalogVersion`; plus the two **row-borne** elements of the D-48 v1 contract — `billingTiming` per recurring row and `taxCategory` per row (D-110) — delivered with the row (Design).

**Compatibility**: Sufficient to render an invoice line and post to GL **without** re-querying mutable catalog rows; the v1 minimum field list is **pinned (D-48, 2026-07-28)**: invoice line template/labels, `taxCategory`, `glCode`, itemization rule, `billingTiming` — of these, `billingTiming` and `taxCategory` ride the price row (D-110) while the other three are descriptor-set fields; additive extension only; Billing countersigns at its gear PRD. `glCode` MAY additionally be governed by a **tenant-declared GL-code vocabulary** checked at publish (**D-356**, 2026-09-09): an empty vocabulary constrains nothing, and the v1 field list is unchanged — ledger stores account *class* and takes the concrete code from this snapshot, so the vocabulary is Catalog's to hold.

#### PriceWindow linkage contract

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-contract-pricewindow`

**Direction**: internal (consolidated per §15 — the window machinery is owned by this PRD's design; this entry records the dissolved boundary for traceability)

**Protocol/Format**: the design owns the window store, state machine, scheduling/cancellation API, the UTC activation job, and `PriceWindowScheduled`/`Activated`/`Cancelled`/`Expired` emission (frozen manifest names); price rows attach windows and publish-time coverage is validated in the same gear/database.

**Compatibility**: Every billable row is window-linkable; retirement runs the same gear's cancellation flow; the canonical scope key (§2.2) is the non-overlap key. Downstream consumers of `PriceWindow*` events are unaffected (names preserved; producer is now this gear).

## 10. Use Cases

#### Guided plan creation

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-usecase-plan-creation`

**Actor**: `cpt-cf-bss-pricing-actor-finance-manager`

**Preconditions**:
- A published SKU exists.

**Main Flow**:
1. Select the published SKU.
2. Choose billing cycle (one-time / recurring / usage / hybrid) and frequency (monthly to custom).
3. Set `PlanTier` and optional ordered phases (trial to evergreen).
4. Configure base price and region/currency scope (per-seat: unit price + quantity source).
5. Add tiers, evaluation policy, or add-on rules.
6. Review the validation summary and submit for approval.

**Postconditions**:
- A complete plan draft is staged, pending approval.

**Alternative Flows**:
- **Fail-closed validation**: ambiguous meter, overlapping tiers, missing `PlanTier`/descriptors, or missing evaluation-policy fields block submission with a field-level report.

#### Tiered pricing editing

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-usecase-tier-editor`

**Actor**: `cpt-cf-bss-pricing-actor-finance-manager`

**Preconditions**:
- A usage plan price row exists.

**Main Flow**:
1. Open the usage plan price row.
2. Add tier rows with bounds and unit prices.
3. Select graduated vs volume.
4. Set `tierAggregationWindow` and `billingGranularity`.
5. Resolve inline validation; save draft or submit.

**Postconditions**:
- A validated, model-kind-explicit tier set is persisted.

**Alternative Flows**:
- **Overlap/gap/non-ascending bands or a closed top band**: save/publish fails (the top band is always open).

#### Hybrid plan composition

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-usecase-hybrid-composer`

**Actor**: `cpt-cf-bss-pricing-actor-finance-manager`

**Preconditions**:
- A usage-capable published SKU exists.

**Main Flow**:
1. Select the usage-capable SKU.
2. Add a recurring base price section.
3. Add a usage price section with tiers or a flat rate.
4. Validate both sections present.
5. Submit for approval.

**Postconditions**:
- A hybrid plan with both components under one `planId` is staged.

**Alternative Flows**:
- **Incomplete hybrid**: publish validation fails until both a recurring and a usage row exist.

#### Plan approval

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-usecase-approval`

**Actor**: `cpt-cf-bss-pricing-actor-finance-reviewer`

**Preconditions**:
- A plan is submitted for publish and flagged material (above threshold or first publish).

**Main Flow**:
1. Open the pending approvals queue.
2. Review the plan diff and threshold flag.
3. Approve or reject with reason.
4. The system records the independent approver (submitter + 1 approver).

**Postconditions**:
- On approval `PlanPublished` is emitted; on rejection the plan returns to `draft` with reason and the submitter is notified.

**Alternative Flows**:
- **Self-approval attempt**: rejected and audit-logged (submitter cannot approve).

#### Plan retirement and migration

- [ ] `p1` - **ID**: `cpt-cf-bss-pricing-usecase-retire-migrate`

**Actor**: `cpt-cf-bss-pricing-actor-finance-manager`

**Preconditions**:
- A plan with active subscriptions and a published target plan exist.

**Main Flow**:
1. View the active subscription count.
2. Choose immediate or scheduled retirement.
3. Select the target plan and migration date.
4. Review cancelled future windows and flagged deltas (contract-locked, entitlement overflow, invalid/missing-required add-ons).
5. If cross-currency/region/frequency: confirm the cancel+new warning (in-place credit forfeited).
6. Confirm the migration event.

**Postconditions**:
- `PlanRetired` (and, on schedule, `PlanMigrationScheduled`) is emitted; Subscriptions creates effective-dated `PlanLink`s; posted invoices are untouched.

**Alternative Flows**:
- **Contract-locked subscribers**: excluded and reported; the lock is never broken.

#### Bulk price import

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-usecase-bulk-import`

**Actor**: `cpt-cf-bss-pricing-actor-finance-manager`

**Preconditions**:
- A valid CSV with plan id, currency, region, amount, and taxInclusive columns.

**Main Flow**:
1. Download the CSV template.
2. Upload the filled file.
3. Review the validation summary.
4. Apply, or fix errors and retry.

**Postconditions**:
- Valid rows commit under optimistic per-row locks; conflicted rows are reported partial for retry.

**Alternative Flows**:
- **Any invalid row**: the whole batch is blocked pre-commit with a per-row report.

#### Price history and export

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-usecase-price-history`

**Actor**: `cpt-cf-bss-pricing-actor-auditor`

**Preconditions**:
- A plan with price change history exists.

**Main Flow**:
1. Open the plan price history.
2. Filter by currency/region/date.
3. View the trend and actors.
4. Export CSV/JSON.

**Postconditions**:
- Chronological immutable records are returned and exported (p95 <= 5s for 100 records).

#### Effective price preview

- [ ] `p2` - **ID**: `cpt-cf-bss-pricing-usecase-price-preview`

**Actor**: `cpt-cf-bss-pricing-actor-partner`

**Preconditions**:
- A published plan and a partner catalog-preview read grant (region/currency) exist.

**Main Flow**:
1. Select the plan in the catalog.
2. The system resolves the base price for the region/currency (fail-closed if no row — no implicit FX).
3. Show the tax-inclusive label, tier summary, and trial length (`displayTrialDays`) if applicable.
4. Show the disclaimer that Tariffs overlays apply at purchase.

**Postconditions**:
- The partner sees the catalog base list price with an overlay disclaimer.

**Alternative Flows**:
- **No row for `(currency, region)`**: preview fails closed (no FX fallback).

## 11. User Interaction and Design

| **Interface Name** | **Role** | **Steps** | **Mockup Screen** |
|--------------------|----------|-----------|-------------------|
| Plan creation wizard | As a Finance Manager, I want a guided flow to define billing cycle, base price, and add-ons so that plans are complete before publish | 1. Select published SKU<br>2. Choose billing cycle (one-time / recurring / usage / hybrid) and frequency (monthly…custom)<br>3. Set PlanTier and optional ordered phases (trial→evergreen)<br>4. Configure base price and region/currency scope (per-seat: unit price + quantity source)<br>5. Add tiers, evaluation policy, or add-on rules<br>6. Review validation summary and submit for approval | — |
| Tiered pricing editor | As a Finance Manager, I want to edit tier bands and model kind so that usage plans rate correctly | 1. Open usage plan price row<br>2. Add tier rows with bounds and unit prices<br>3. Select graduated vs volume<br>4. Set tier aggregation window and billing granularity<br>5. Inline validation; save draft or submit | — |
| Hybrid plan composer | As a Finance Manager, I want recurring base plus usage on one plan so that hybrid SKUs are modeled once | 1. Select usage-capable SKU<br>2. Add recurring base price section<br>3. Add usage price section with tiers or flat rate<br>4. Validate both sections present<br>5. Submit for approval | — |
| Billing descriptors panel | As a Finance Manager, I want to set invoice and GL fields on the plan so that Billing renders lines correctly | 1. Open plan publish checklist<br>2. Complete invoice template, tax category, GL code<br>3. Set bundle/add-on itemization if applicable<br>4. Resolve validation gaps | — |
| Add-on configuration | As a Product Manager, I want add-ons with dependency rules so that subscription composition is constrained | 1. Open plan detail<br>2. Search published add-on SKUs<br>3. Set required/optional and quantity bounds<br>4. Optional price override<br>5. Save and validate compatibility | — |
| Bundle builder | As a Product Manager, I want bundles with rev-share so that marketplace packages are sellable | 1. Select included SKUs and quantities<br>2. Set min/max bundle constraints<br>3. Configure rev-share and itemization<br>4. Set bundle price; review savings vs sum of parts<br>5. Publish bundle | — |
| Plan approval dashboard | As a Finance Reviewer, I want to approve or reject plan publishes so that material pricing changes are controlled | 1. Open pending approvals queue<br>2. Review plan diff and threshold flag<br>3. Approve or reject with reason<br>4. Track the independent approver when required (submitter + 1 approver) | — |
| Plan retirement and migration | As a Finance Manager, I want to retire a plan and schedule migration so that legacy pricing sunsets safely | 1. View active subscription count<br>2. Choose immediate or scheduled retirement<br>3. Select target plan and migration date<br>4. Review cancelled future price windows AND flagged deltas (contract-locked, entitlement overflow, invalid/missing-required add-ons)<br>5. If cross-currency/region/frequency: confirm the cancel+new warning (in-place credit forfeited)<br>6. Confirm migration event | — |
| Price history and export | As an Auditor, I want price history so that I can reconcile contract and billing disputes | 1. Open plan price history<br>2. Filter by currency/region/date<br>3. View trend and actors<br>4. Export CSV/JSON | — |
| Bulk price import | As a Finance Manager, I want bulk price updates so that annual adjustments are efficient | 1. Download CSV template<br>2. Upload filled file<br>3. Review validation summary<br>4. Apply or fix errors and retry | — |
| Effective price preview | As a Partner, I want list price preview so that I can quote customers | 1. Select plan in catalog<br>2. System resolves base price for region/currency (fail-closed if no row — no implicit FX)<br>3. Show tax-inclusive label, tier summary, and trial length (`displayTrialDays`) if applicable<br>4. Disclaimer for Tariffs overlays | — |

## 12. Acceptance Criteria

> **As a** Finance Manager or Product Manager **I want** to model subscription plans and price structures in the catalog **so that** Subscriptions can sell them and Rating can charge deterministically.

### Plan definition and billing cycles

**1. Create recurring monthly plan**
- **Given** a published SKU without usage metering (or with metering ignored for pure recurring)
- **When** a Finance Manager creates a Plan with `billingCycle=recurring`, frequency `monthly`, and a base Price row
- **Then** the system MUST store the Plan in `draft` (`PlanTier` is optional at draft and REQUIRED at publish — AC #7; it is not an input to this creation step, so creation MUST NOT demand it)
- **And** MUST validate non-negative amount (>= 0; `0` allowed for free/trial tiers), ISO 4217 currency, and active region scope

**2. Create recurring annual plan**
- **Given** a published SKU and FinanceReviewer role
- **When** a Finance Manager creates a Plan with frequency `annual` and base Price row
- **Then** the system MUST persist annual billing cycle metadata on the Plan
- **And** MUST allow independent price rows per currency/region for the same plan

**3. Usage-based plan with pricing tiers**
- **Given** a published SKU with `meteringUnit` configured
- **When** a Finance Manager configures tier bands and selects `modelKind` `graduated` or `volume`
- **Then** the system MUST reject overlapping quantity ranges
- **And** MUST persist tier definitions, `modelKind`, `tierAggregationWindow`, and `billingGranularity`

**4. One-time plan constraints**
- **Given** a Plan with `billingCycle=one-time`
- **When** add-on rules include recurring-only add-on SKUs
- **Then** publish validation MUST fail with a clear business error
- **And** MUST NOT emit `PlanPublished` until resolved

**5. Availability dates and one-time quantity**
- **Given** a Plan of any billing cycle (purchasability dating is the unified `availableFrom`/`availableTo` pair)
- **When** a Finance Manager sets `availableFrom`/`availableTo` (and, on a one-time Plan, min/max purchase quantity)
- **Then** the system MUST reject `availableFrom` in the past (relative to **UTC** per manifest) for new plans — **with no exception**, ~~except via an explicit historical-import path that MAY set past dates (AC #65)~~, that path being struck by D-330 (2026-08-16)
- **And** MUST reject `minQty > maxQty`

**6. Hybrid plan (recurring + usage)**
- **Given** a published SKU with `meteringUnit`
- **When** a Finance Manager defines a hybrid Plan with both a recurring base Price and a usage Price row
- **Then** publish validation MUST pass only when both components exist
- **And** the read model MUST expose both rows under one `planId` for Tariffs hybrid evaluation

### Plan composition, phases, and descriptors

**7. Mandatory PlanTier**
- **Given** a Plan in `draft` without `PlanTier`
- **When** publish is requested
- **Then** publication MUST be blocked
- **And** the operator MUST receive a field-level validation message
- **And** publish MUST **also** block when the Plan's `PlanTier` differs from its parent SKU's `PlanTier` unless an explicit, audited override is set (plan-vs-SKU tier consistency)

**8. Plan phase price schedule (catalog)**
- **Given** a recurring Plan with configured, ordered phases (trial, interim, evergreen)
- **When** the Plan is published
- **Then** the catalog read model MUST map each phase id to applicable price row references and persist phase ordering with each phase's `convertsToPhaseId` successor
- **And** Subscriptions MUST be able to resolve phase→price and trial-to-paid binding without reading draft state
- **And** trial **runtime** mechanics (payment-method requirement, auto-convert vs. expire — enforcing the catalog-published `phaseDurationDays`/`displayTrialDays`) remain owned by Subscriptions Trial Management

**9. Billing descriptor completeness**
- **Given** a Plan submitted for publish
- **When** invoice line template, tax category, or GL mapping is missing per manifest minimum set
- **Then** publication MUST be blocked
- **And** the validation report MUST list missing descriptor fields

**10. Bundle itemization mode**
- **Given** a Bundle with multiple included SKUs
- **When** a Product Manager sets `invoiceItemization=aggregate` or `itemize`
- **Then** the system MUST persist the mode on the Bundle
- **And** published snapshots MUST include itemization rule for Billing line generation

### Pricing tiers and usage policy

**11. Graduated vs volume distinction**
- **Given** identical numeric tier bands, including a `$0` first band (`fromQty=0`)
- **When** one price row is `modelKind=graduated` and another is `modelKind=volume`
- **Then** both MUST publish successfully
- **And** the read model MUST persist distinct model kinds for Tariffs (no inference at rating time)
- **And** a total quantity falling entirely within the `$0` first band MUST resolve to a `0` charge under both kinds (no implicit minimum)

**12. Tier band validation**
- **Given** tier bands on a usage price row following the `[fromQty, toQty)` convention
- **When** bands overlap, leave coverage gaps, or `fromQty` is not ascending
- **Then** save/publish MUST fail
- **And** the top band MUST be **open-ended** (`toQty=null`) — a closed top band MUST fail publish (quantity capping is owned by entitlement quotas; per-period fee caps are Tariffs Future scope), so any quantity is always rateable on a tiered row
- **And** SHOULD warn (advisory, non-blocking) when any band's effective unit price exceeds the previous band's (non-volume-discount pattern)

**13. Tier aggregation window required**
- **Given** a tiered usage price row — **or a `modelKind=package` usage row** (D-58: block round-up is non-linear in the accumulation window, so `package` carries it exactly like a tiered row)
- **When** `tierAggregationWindow` is unset at publish
- **Then** publication MUST fail (`EVAL_POLICY_MISSING`)
- **And** the error MUST reference allowed values per Glossary

**14. Billing granularity required**
- **Given** a usage price row (tiered or untiered)
- **When** `billingGranularity` is unset at publish
- **Then** publication MUST fail
- **And** the published row MUST be consumable by Tariffs step 3

**15. Untiered usage unit price**
- **Given** a usage SKU without tier bands
- **When** a Finance Manager sets `modelKind=per_unit` and unit price
- **Then** publish MUST succeed with single-rate read model
- **And** tier fields MUST remain empty and `quantitySource` MUST be absent (the meter supplies `Q`), but `billingGranularity` is still REQUIRED (per AC #14) — untiered usage is not exempt from evaluation-policy fields
- **And** `modelKind=flat` on a usage row MUST fail publish (`MODEL_KIND_CHARGEKIND_MISMATCH`) — `flat` is a non-usage kind (D-18 matrix, D-57, S3 `inst-mk-chargekind`)

### Multi-currency, tax display, and price rows

**16. Multiple currencies on one plan**
- **Given** an existing Plan with USD price for region US
- **When** a Finance Manager adds EUR price for region EU on the same plan
- **Then** both price rows MUST be stored linked to the same `planId`
- **And** MUST emit `PriceCreated` per row (frozen event name — see §4 Event alignment; no aliases)

**17. Duplicate scope-key rejection**
- **Given** a Plan with a published price row on a canonical scope key `(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)`
- **When** a second row is created on the **same** canonical scope key without the supersession workflow
- **Then** the system MUST reject the duplicate
- **And** MUST suggest update of the existing row; rows differing on **any** key axis — e.g. a `trial`-phase price, an `existing_grandfathered` row (AC #25), or the `usage` vs `recurring` component of a hybrid plan — are **distinct**, not duplicates

**18. Tax-inclusive flag**
- **Given** a price row with `taxInclusive=true` for a region with configured tax rate
- **When** the price is displayed or published
- **Then** the system MUST persist `taxInclusive=true` on the snapshot
- **And** MUST show tax-included labeling in preview UI; tax calculation remains downstream
- **And** because Tax Engine is **post-MVP** (§13 External dependencies), at MVP a `taxInclusive=true` price row MAY be authored but MUST be flagged **not-sellable-GA** until Tax Engine GA — per row / `(currency, region)` market, gating only the tax-inclusive markets of a mixed plan (tax-inclusive validation + `region`→jurisdiction depend on it); MVP sells tax-exclusive

**19. Tax-inclusive without tax rate**
- **Given** `taxInclusive=true` for a region without tax rate configuration
- **When** publish is requested
- **Then** publication MUST fail or require explicit override per the **tenant tax-display policy** (default fail-closed; see §17.6 Tenant policy objects)
- **And** tax **scheme** determination (VAT/GST/US sales tax) remains owned by Tax Engine — catalog persists only display basis and `taxCategory`

### Add-ons and bundles

**20. Add-on dependency rules**
- **Given** a base Plan and published add-on SKUs
- **When** add-on rules are saved with required/optional and min/max/step
- **Then** the system MUST validate SKU compatibility with the base SKU
- **And** MUST reject before publish a conflicting pair in which **both add-ons are required** (unsatisfiable); a conflict between optional (or mixed required/optional) add-ons publishes and is enforced by Subscriptions at selection time

**21. Required add-on quantity bounds**
- **Given** an add-on marked required
- **When** `maxQty=0` is configured
- **Then** validation MUST fail
- **And** MUST NOT allow save

**22. Add-on price override reference**
- **Given** an add-on rule with catalog price override (discounted vs standalone)
- **When** the rule is saved
- **Then** the system MUST persist override reference on the plan snapshot
- **And** SHOULD warn (advisory, non-blocking) when the override unit price exceeds the add-on's standalone list price

**23. Bundle with rev-share**
- **Given** a marketplace Bundle with vendor and platform SKUs
- **When** rev-share percentages are configured (parties = the platform cut + one or more vendor shares)
- **Then** all shares MUST sum to 100% per included vendor SKU, with the platform cut being one **explicit** share (not an implicit remainder)
- **And** the operator MUST nominate a **primary party per vendor-SKU group** at bundle authoring (default = the platform; a party row of that group, since the residual is computed per group) that absorbs the group's rounding residual (e.g. 33.33%×3) within a <= 0.01% (1 bp) tolerance; publish MUST **normalize** the absorber's effective share so the published shares sum to exactly 100% (typed values audited, the adjustment recorded); a residual over tolerance MUST fail publish
- **And** rev-share is scoped per `(bundle, vendor SKU)` — the same SKU in multiple bundles carries independent shares; `invoiceItemization=aggregate` MUST still preserve per-SKU rev-share for Marketplace accrual

### PriceWindow linkage

**24. Publish requires window coverage**
- **Given** a billable Plan with price rows for `(currency, region)` but no active or scheduled `PriceWindow`
- **When** publish is requested
- **Then** publication MUST fail for production sellability
- **And** MUST direct the operator to schedule a window via the window-scheduling API (owned here since the consolidation)
- **And** for **one-time** plans the two effectivity mechanisms are distinct and both apply: a **`PriceWindow`** governs **price effectivity** (a purchase at `t` MUST have a covering window) while `availableFrom`/`availableTo` govern **purchasability** — when they disagree, purchase requires **both** an active window **and** an open availability interval (AC #5)

**25. Grandfathering eligibility on window/price**
- **Given** a price row with `priceEligibility=existing_grandfathered` and cutover timestamp
- **When** published
- **Then** the read model MUST expose eligibility metadata for Tariffs step 2
- **And** new subscriptions after cutover MUST NOT bind to grandfathered row
- **And** the grandfathered row and its successor row are **distinct canonical scope keys** (they differ by `priceEligibility`) and both MAY hold active `PriceWindow`s at the same `t` without violating non-overlap; Tariffs step 2 selects by `priceEligibility` (most-specific-wins), and within the `existing_grandfathered` class by the **`cohort` of the subscription's pinned price id** (`pricingSnapshotRef`) — never by `activatedAt` (ADR-0002)

### Publish validation, approval, and events

**26. Publish validation fail-closed (aggregate)**

> **Scope of this list (D-75).** The conditions below are **illustrative, not exhaustive**: the normative fail-closed set is §17.4 plus every rule a slice design registers into the validation pipeline, and a condition's absence here is never permission to publish. The list is maintained on a best-effort basis for reviewers; where it and a slice validator disagree, **the validator governs**. Do not cite AC #26 as a complete error enumeration.

- **Given** a Plan submitted for publication
- **When** validation detects any of: ambiguous meter, overlapping tiers, tier coverage gaps, **unspecified `modelKind` on a tiered row**, a `graduated`/`volume`/`package` kind on a non-usage row, evaluation-policy fields present on a non-usage row, a `package` row with non-positive `packageSize`/negative `packagePrice`/tier bands present **or with no `tierAggregationWindow`** (D-58), a non-`sum` `aggregationFunction` row with invalid level fields (unknown value, `maxHold` missing or < 1 on a non-`sum` row, `maxHold` present on a `sum` row), a non-`sum` row whose meter is not gauge-kind or whose billable unit mismatches `level unit × granule`, a non-`sum` row whose `billingGranularity` does not pair with its `aggregationGranularity` (D-77), a non-`sum` row on a derived (composite) meter (D-44), a prepaid grant with unset `expiryPolicy` or an unpublished `creditUnit`, a `promotional` grant carrying price rows or `autoRechargeAllowed`, a grant `applicability` naming an unpublished meter or a non-usage line (or, for a metered `creditUnit`, leaving that unit's meters), missing PlanTier, missing descriptors, invalid hybrid, missing evaluation-policy fields, currency precision exceeding the ISO 4217 minor unit, an unknown `region`, an unknown scope value on a scoped `PriceOverlay` (`brand`/`region`/`partner`/`orgTier` against their tenant taxonomies — D-120), an add-on dependency cycle, an uncovered gap between scheduled `PriceWindow` rows, a `customEveryN Days` cycle with a `calendar_month`/`fixed_day` anchor, a `PriceOverlay` adjustment with an ambiguous tax basis, an amount-based `PriceOverlay` **line** missing a value for a currency its target scope sells (or a duplicate/out-of-scope overlay line key — D-42, or an out-of-range line magnitude — D-67), a rev-share reconciliation forcing a negative share, a configuration forcing mixed-currency lines on one invoice, a recurring row missing `billingTiming`, a one-time setup row that is not one-time (has recurrence, `billingTiming`, or tier fields), a `minQtyThreshold` with no declared floor type (or a `usage` floor with no declared fallback), a plan `PlanTier` unequal to its SKU `PlanTier` without an audited override, an `availableFrom`/`availableTo` not covered by an active or scheduled window, a dangling or cyclic `convertsToPhaseId` (or a non-terminal phase without `phaseDurationDays`, or not exactly one terminal phase), on a `recurring`/`hybrid` plan a phase with no covering recurring price row for a sold `(currency, region)` (D-74), a revision re-terminalizing a phase or dropping one still referenced by a published row (D-64), `creditOnDowngrade = true` combined with `prorationBasis = none`, an unresolvable rounding policy (neither row-level nor tenant default), an `allowedChangeTargets` entry that is unpublished or lacks a `comparabilityRank`, an entitlement grant set with an undefined feature/quota/`PlanTier` policy, a `prepaid`-category grant price that is unscoped or does not cover every sold `(currency, region)`, a derived meter with an unpublished constituent or a self-reference, a non-positive custom interval `n`, or a **required** scope key with no covering price row — the *required coverage set* = every canonical scope key that (a) has a scheduled `PriceWindow`, (b) is a `sum_of_parts` bundle component's sell scope, or (c) is an add-on/override target for a scope the base plan sells
- **Then** publication MUST be blocked
- **And** MUST NOT emit `PlanPublished` or warm Rating read models

**27. Successful publish fan-out**
- **Given** a Plan passing validation and required approvals
- **When** publication completes
- **Then** the system MUST emit `PlanPublished` carrying a **pending** `CatalogVersion` reference (the addressable version is committed by the registry, which MAY batch — see §17.5 CatalogVersion increment contract); the committed `CatalogVersion` and the `pricingSnapshotRef` composite are finalized when the registry emits `CatalogVersionPublished`
- **And** Rating/Tariffs MUST resolve `{skuId, planId, priceId}`, model kind, and evaluation-policy fields from the committed `CatalogVersion` without draft reads

**28. Two-person rule on material change**
- **Given** a material change — a price change above the configured threshold, **or** a first publish with no prior baseline (always material, per Glossary)
- **When** a Finance Manager submits for publish
- **Then** the system MUST require **one independent approver** before `PlanPublished` (submitter + 1 approver = **two distinct principals**; the submitter MUST NOT approve — AC #70)
- **And** MUST log the submitter and the approver identities and timestamps

**29. Approval rejection**
- **Given** a pending plan approval
- **When** a Finance Reviewer rejects with reason
- **Then** the Plan MUST return to `draft`
- **And** MUST emit audit event with rejection reason; submitter MUST be notified

**30. Auto-publish below threshold**
- **Given** a plan change and a tenant approval-threshold policy
- **When** publish is requested
- **Then** the system MUST apply the two-person rule **by default** (see AC #28; auto-publish is the **only** exception), and MAY auto-publish with **no independent approver** **only if** a threshold is explicitly configured **and** the change is below it **and** it is not a first publish (no configured threshold, or a first publish, implies two-person rule always applies)
- **And** in either path the system MUST emit `PlanPublished` and an audit record

**31. Pricing snapshot on subscription artifacts**
- **Given** an active subscription from a published Plan
- **When** Subscriptions or Rating generates charge artifacts
- **Then** artifacts MUST carry catalog refs and `pricingSnapshotRef` per manifest §4.1
- **And** posted invoice periods MUST NOT re-query mutable catalog rows

### Plan lifecycle, update, and migration

**32. Plan update with active subscribers**
- **Given** a published Plan with active subscriptions
- **When** price or tier configuration changes
- **Then** the system MUST version the Plan and create new Price records (immutable history)
- **And** existing subscriptions MUST continue on frozen snapshot until renewal or migration
- **And** exception: subscriptions bound via `existing_grandfathered` are **live-resolved** by Tariffs against an **immutable** grandfathered row (MUST NOT be superseded; sunset only via `grandfatherUntil` — see §17.5), so their price stays stable and consistent with the frozen-snapshot doctrine

**33. Plan retirement — block new, preserve existing**
- **Given** a Plan with active subscriptions
- **When** immediate retirement is executed
- **Then** new subscriptions to that `planId` MUST be blocked
- **And** existing subscriptions MUST retain plan snapshots; catalog MUST emit `PlanRetired`

**34. Scheduled migration to alternative plan**
- **Given** a retiring Plan and published target Plan
- **When** migration is configured with effective date and target `planId`
- **Then** the system MUST emit the migration-scheduled event **`PlanMigrationScheduled`** (frozen event name — see §4 Event alignment; no aliases) for Subscriptions
- **And** Subscriptions MUST create effective-dated `PlanLink` without posted invoice mutation

**35. Retirement cancels future price windows**
- **Given** a Plan with scheduled future `PriceWindow` rows
- **When** the Plan is retired
- **Then** the system MUST cancel not-yet-active windows **only for scope keys with no in-flight subscribers (D-51, 2026-07-28)** and MUST **run the window-cancellation flow, which emits `PriceWindowCancelled` per cancelled window** (frozen manifest event name; produced by this PRD's design since the consolidation; drives the cache-eviction path, <= 2s NFR) — it MUST NOT merely mark windows invalid without the cancellation flow
- **And** a scheduled window that is the **continuing coverage** of a scope key **with** in-flight subscribers MUST be **kept**, not cancelled (D-51) — retirement stops **selling** via the sellability gate's lifecycle predicate, never **rating** coverage (cancelling it would open a trailing void no gap-check can see); the confirm screen MUST label kept vs cancelled windows
- **And** a pending or approved-not-yet-effective grandfathering cutover on the plan's keys MUST be **unwound** in the same transaction: the predecessor window's `effectiveTo` restored to its pre-cutover value, the scheduled copy/successor windows cancelled, the unit closed as unwound (a merely `submitted` unit is voided per the standard pin semantics) — otherwise the shortened window would strand in-flight subscribers uncovered at the cutover instant
- **And** retirement of a plan holding a live cutover unit is **always material** (two-person rule)
- **And** MUST warn the operator of cancelled windows — and of any cutover to be unwound — before confirm

**36. Contract-locked plan**
- **Given** an active contract referencing a Plan revision
- **When** structural plan mutation is attempted
- **Then** the system MUST reject while lock is active
- **And** MUST direct operator to new Plan revision or contract expiry

**37. Concurrent edit detection**
- **Given** two Finance Managers editing the same Plan
- **When** the second submits with stale version/ETag
- **Then** the system MUST reject with conflict
- **And** MUST require refresh before retry
- **And** the token MUST be **per price row**, never derived from the plan's, and **every** mutating verb on a `draft` price row MUST present it — **`DELETE` included** (`STALE_VERSION`, 409; **D-141**, 2026-08-02 implementation finding): with `DELETE` unconditional a draft row is destroyed under an unknown version, losing a concurrent editor's uncommitted work with nothing left behind to reconcile, and a per-row bulk conflict (AC #39) is inexpressible under a shared plan version

### Operator efficiency

**38. Plan clone**
- **Given** a source Plan (draft or published)
- **When** clone is executed with new name
- **Then** the system MUST create a new `planId` in `draft` with copied configuration and new price ids
- **And** MUST record `clonedFrom` reference; MUST NOT affect source subscriptions

**39. Bulk price import**
- **Given** a valid CSV with plan id, currency, region, amount, taxInclusive columns
- **When** bulk import is applied
- **Then** **validation** MUST be all-or-nothing **pre-commit** (any invalid row blocks the whole batch with a per-row report)
- **And** at **commit**, the system MUST take optimistic per-row locks; a per-row ETag conflict (concurrent manual edit) MUST fail **only that row** and the batch MUST be reported **partial** (committed rows stand; conflicted rows are listed for retry) — it MUST NOT silently overwrite

**40. Price history and export**
- **Given** a Plan with price change history
- **When** an Auditor requests history with filters
- **Then** the system MUST return chronological immutable records with actor and effective dates
- **And** MUST support export within p95 <= 5s for 100 records
- **And** history/audit list reads MUST be cursor-paginated with a stable commit order — a page walk concurrent with writes never skips or duplicates a row at or before the cursor (D-125)

### Price preview and access control

**41. Catalog base price preview**
- **Given** a published Plan and partner context (region, currency)
- **When** preview is requested
- **Then** the system MUST return catalog base price, taxInclusive flag, and tier summary if applicable
- **And** MUST indicate that Contract/PriceOverlays apply at purchase (Tariffs)

**42. RBAC deny-by-default**
- **Given** a user without CatalogAdmin, FinanceReviewer, ProductManager, or Auditor role
- **When** plan mutate APIs are called
- **Then** access MUST be denied
- **And** attempts MUST be audit-logged
- **And** **read/preview** APIs MUST also be **deny-by-default**: an unlisted-role principal (e.g. a Partner/SI holding none of CatalogAdmin/FinanceReviewer/ProductManager/Auditor) MUST be denied unless it holds an explicit **catalog-preview read grant** (a scoped role/claim); partner preview (AC #41) requires this grant, region/brand-scoped by IdP claims (AC #43)
- **And** ~~the **historical-import (backdating) capability** is a distinct restricted grant (a scoped `CatalogAdmin` capability, NOT a default role) required by AC #65~~ — **struck by D-330** (2026-08-16): no such capability is issued, and the resource it targeted is not registered

**43. Tenant and brand isolation**
- **Given** two tenants with plans configured
- **When** a user queries or mutates plans
- **Then** the system MUST enforce tenant isolation
- **And** MUST scope brand/region per IdP claims at gateway (authorization/display scope; pricing `region` is decoupled — see AC #68); here `brand` is an **authorization/display** scope, **not** a price-row key (see §2.2 Canonical scope key)

### Commercial model coverage

**44. Per-seat (per-unit recurring) plan**
- **Given** a published SKU and a recurring Plan priced per unit (seat/user)
- **When** a Finance Manager sets `modelKind=per_unit`, a unit price, and `quantitySource` (`subscription_seat_count` \| `manual`)
- **Then** publish MUST persist the unit price and quantity source
- **And** the read model MUST let Rating/Tariffs resolve per-period quantity **from the source declared by `quantitySource`** — from Subscriptions when `subscription_seat_count`, or from the fixed value frozen on the row when `manual` — without inferring or metering it (no metering-unit footprint)

**45. Custom billing frequency**
- **Given** a recurring Plan
- **When** a Finance Manager sets `frequency` to `quarterly`, `semiannual`, or `customEveryN{Days|Months}(n)`
- **Then** the system MUST persist the interval as metadata
- **And** MUST reject a non-positive `n`
- **And** a `customEveryN Months(n)` `subscription_start` anchor beyond the target month's length MUST clamp to the month's last day with the anchor day preserved across periods (Jan 31 → Feb 28 → Mar 31 — no drift; UTC)

### Proration and provisioning contracts

**46. Proration input contract (catalog → Subscriptions)**
- **Given** a recurring price row submitted for publish
- **When** publication completes
- **Then** the read model MUST expose `billingAnchorPolicy`, `prorationBasis`, and `creditOnDowngrade`
- **And** these fields MUST be frozen in `pricingSnapshotRef`; proration math remains downstream (rating gear; Subscriptions sets the boundary/mode)
- **And** a mid-cycle change crossing currency, region, or billing frequency MUST be rejected for in-place proration (handled as cancel + new subscription; see §17.6 Proration input contract)

**47. Entitlement grant set in published read model**
- **Given** a Plan submitted for publish
- **When** validation runs
- **Then** the published read model MUST include the plan's entitlement grant set (feature flags, quotas) or its `PlanTier`-resolved reference for Subscriptions provisioning
- **And** publish MUST fail if a referenced feature, quota, or `PlanTier` policy is undefined in the registry

### Currency and FX policy

**48. Missing currency row fails closed (no implicit FX)**
- **Given** a preview or publish for a `(currency, region)` with no price row
- **When** resolution runs
- **Then** the system MUST fail closed and MUST NOT compute or apply FX conversion
- **And** a base-currency fallback MUST occur only when an explicit `currencyFallbackPolicy` is configured (Future scope)

### Lifecycle safety (contract lock and entitlement overflow)

**49. Scheduled migration vs contract-lock and entitlement overflow**
- **Given** a scheduled migration to a target Plan
- **When** some source subscriptions are contract-locked, or the target Plan's entitlement limits are below current usage/seat counts
- **Then** the system MUST exclude contract-locked subscriptions from the migration and report them, and MUST NOT break an active contract lock
- **And** the migration config MUST surface entitlement deltas and warn/block when target limits < entitlements in use; runtime enforcement is owned by Subscriptions
- **And** the migration config MUST flag subscribers whose current add-ons become invalid under the target plan's add-on rules (e.g. a now-incompatible add-on), AND subscribers who **lack a required add-on** declared by the target plan, reporting both as blocking deltas alongside entitlement deltas

### Read-model and publish robustness

**50. Consumer-side read-model resolution (determinism)**
- **Given** a published Plan with `modelKind=graduated`
- **When** Tariffs or Rating resolves the price input via `pricingSnapshotRef`
- **Then** it MUST receive the exact ordered tier bands, `tierAggregationWindow`, and `billingGranularity` as published
- **And** it MUST NOT read draft state and MUST NOT substitute any default for an absent evaluation-policy field (absence MUST have failed publish)

**51. Publish fan-out atomicity / compensation**
- **Given** a Plan passing validation and approval
- **When** `PlanPublished` is emitted but read-model warming or a downstream notification fails
- **Then**, once the version commits (`CatalogVersionPublished`), the system MUST either complete warming within the 5s SLO via retry, or mark the publish degraded and emit a `PlanPublishDegraded` signal; the pre-commit batching delay (`PlanPublished` → `CatalogVersionPublished`) is governed by the AC #95 max batching-delay SLO, **not** by `PlanPublishDegraded`
- **And** the system MUST NOT leave a state where charges can resolve against a partially-published plan

**52. Currency precision per ISO 4217 minor units**
- **Given** a price row in a currency with a non-2 minor unit (e.g. JPY=0, BHD=3)
- **When** an amount is saved with more precision than the currency's ISO 4217 minor unit
- **Then** publish MUST reject it
- **And** a `0`-minor-unit currency MUST reject any fractional amount; a 3-minor-unit currency MUST accept up to 3 decimals

**53. Billing/ERP descriptor sufficiency**
- **Given** a published billing descriptor set
- **When** Billing/ERP consumes the snapshot to render an invoice line and post to GL
- **Then** the descriptor set MUST contain the agreed minimum field set (invoice template, tax category, `glCode`, itemization rule) sufficient to post **without re-querying mutable catalog rows**
- **And** publish MUST fail if any element of that agreed minimum set is absent (the v1 list is **pinned — D-48, 2026-07-28**: invoice line template/labels, `taxCategory`, `glCode`, itemization rule, `billingTiming`; three are descriptor-set fields while **`billingTiming` and `taxCategory` ride the price row** — D-110, since a tax category is per row and a per-plan descriptor column could not mirror it; additive extension only)
- **And** when the tenant has declared a GL-code vocabulary, a present `glCode` outside it MUST fail publish with `GL_CODE_UNKNOWN`; an empty vocabulary MUST constrain nothing (**D-356**, 2026-09-09 — an optional membership check on one v1 element; the pinned list is unchanged)

### Phases and PriceOverlay authoring

**54. Phase ordering integrity**
- **Given** a Plan with ordered phases referencing each other via `convertsToPhaseId`
- **When** publish runs
- **Then** the system MUST reject any `convertsToPhaseId` that is dangling (targets a non-existent phase) or forms a cycle
- **And** MUST require exactly one terminal phase (no successor, e.g. `evergreen`)
- **And** on a plan carrying a **recurring part** (`recurring`/`hybrid`) MUST reject a phase with no covering published recurring price row for a sold `(currency, region)` (an uncovered phase would rate to nothing at conversion); one-time and usage-only plans are outside the rule (D-74) and are guarded instead by terminal-phase immutability across revisions (D-64)
- **And** usage rows resolve phase-invariantly by default (one usage row covers all phases; an explicit phase-scoped usage row wins for its phase — adopted verbatim by Tariffs)

**55. PriceOverlay authoring and precedence**
- **Given** a Catalog Admin authoring a `PriceOverlay` (an **adjustment-line container**, D-42)
- **When** scope (partner/orgTier/brand/region/**customerGroup**/global), one or more adjustment **lines** — each keyed `(planId?, targetSku?)` with its own type (markup/discount/fixed) and magnitude — and an explicit `precedence` value are set
- **Then** the system MUST persist the overlay and its lines, MUST reject a **duplicate line key** or an **out-of-scope line target** (fails publish — D-42), and MUST reject a duplicate `precedence` within the same scope class (deterministic ordering for Tariffs); the **most-specific line wins within a list** (`(planId, targetSku)` > `(planId)` > list-default)
- **And** an **amount-based** line MUST carry per-currency values covering every currency the line's target scope sells (fail-closed at authoring; a later-added currency flags the **line** for remediation while the uncovered market resolves at base price — D-08, re-attached per line); a percent line is currency-neutral
- **And** a line MUST NOT apply to an `existing_grandfathered` row unless it explicitly names that row's `cohort` (D-78 — grandfathered generations are exempt from the stack by default; a `cohort` no published grandfathered row of the target plan carries MUST fail publish, `OVERLAY_LINE_COHORT_UNKNOWN`); Tariffs adopts the filter verbatim
- **And** every magnitude MUST be range-bounded (D-67): a `discount` percent line in `(0, 10000]` bp, a `markup` percent line `> 0` bp, an amount-based magnitude `>= 0` at its currency's minor unit — an out-of-range value MUST fail save/publish with `ADJUSTMENT_MAGNITUDE_OUT_OF_RANGE` naming the line (the resolved **stacked** floor — what happens when a legal stack drives the price below zero — is a separate open product fork, §15)
- **And** precedence/stacking **evaluation** remains owned by Tariffs (authoring/validation only here)
- **And** a `PriceOverlay` adjustment MAY be **effective-dated** via its own **adjustment-level effectivity interval** `[effectiveFrom, effectiveTo)` — **outside** the per-plan `PriceWindow` mechanism, since an adjustment is scope-wide and has no single `planId`/`phase`/`priceEligibility`/`chargeKind`; absent an interval it applies whenever the base row is active. Overlap is validated **per `PriceOverlay` scope + adjustment target** at authoring (not on the canonical price-row key). Base price rows always carry `priceOverlay = base` (see §2.2)

**56. PriceOverlay references unpublished plan/SKU**
- **Given** a `PriceOverlay` row whose scope references a plan or SKU that is not published
- **When** the row is saved or the list is published
- **Then** the system MUST reject it with a clear business error
- **And** MUST NOT expose the row in the read model

### Migration robustness and eligibility

**57. Migration idempotency and cancellation**
- **Given** a scheduled migration
- **When** the migration trigger is re-executed (retry) or the operator cancels before the effective date
- **Then** retry MUST be idempotent (no duplicate `PlanLink` requests for already-processed subscriptions)
- **And** cancellation MUST invalidate the scheduled migration event without affecting already-migrated subscriptions

**58. Legacy subscription migration without prior snapshot**
- **Given** a legacy subscription with no `pricingSnapshotRef`
- **When** it is migrated or first rated under this system
- **Then** the system MUST synthesize and freeze a snapshot from the published plan state **as of the trigger instant (UTC), frozen at execution** (not config time), recording it as `migrated-origin`: for the `migration` trigger the instant is the **migration effective timestamp**; for the **first-rating** trigger (no prior snapshot, not a migration) it is the **earliest unrated usage timestamp (UTC)** for the subscription — two implementations MUST freeze identical prices for the same subscription
- **And** MUST persist a provenance record containing at minimum: source `planId`/revision, resolved price ids, the snapshot instant (UTC), the trigger (`migration` \| `first-rating`), the acting principal, and **per resolved id the selection tier** (`live_history` — D-76's second value `historical_import` is struck with the import flow, D-330, 2026-08-16; the field stays, an auditor still needing to see which rule resolved the row) (exact field shape MAY be finalized in Design; this field list is normative)
- **And** each row MUST be resolved by the live-history rule (D-76 as narrowed by D-330): the live/superseded row whose window covered the instant, else **fail closed** — never the current row. ~~Tier 2, the backdated reference row whose own effective interval covers the instant~~, is **struck**
- **And** ~~the snapshot MAY reference a backdated price row created via the historical-import path (AC #65)~~ — **struck by D-330**; the snapshot references only rows this catalog published, and it MUST NOT silently rate against mutable catalog rows

**59. `new_subscriptions_only` eligibility binding**
- **Given** a price row with `priceEligibility=new_subscriptions_only` and a cutover timestamp
- **When** published
- **Then** new subscriptions created on/after cutover MUST bind to the row
- **And** existing subscriptions MUST NOT be re-bound to it (they retain their prior snapshot)

### Cross-PRD contract conformance

**60. Tariffs tier-boundary conformance**
- **Given** a published tier set under the `[fromQty, toQty)` half-open convention
- **When** Tariffs evaluates a charge at each band boundary value (including the `fromQty=0` `$0` band and the open-ended top band)
- **Then** the charge MUST match a jointly-owned golden conformance fixture exactly (no off-by-one at band edges)
- **And** publish MUST be blocked if the conformance fixture is absent for a newly introduced `modelKind`
- **And** `package` (repeating-block) and `per_unit` (external-quantity) MUST **each** have a joint golden fixture before publish (per §17.2); absent the fixture, plans of that kind MUST NOT publish (two Scope items stay unsellable until Tariffs ships the semantics)

**61. Proration field-consumption contract**
- **Given** each proration input field (`billingAnchorPolicy`, `prorationBasis`, `creditOnDowngrade`)
- **When** the publish contract is validated
- **Then** each field MUST map to a named consuming requirement in Subscriptions (Proration Logic) **and** in Tariffs, which consumes the **same** `prorationBasis` for PriceWindow-split / plan-change proration and MUST adopt the canonical enum **verbatim**
- **And** a shared golden fixture MUST assert that `calendar_days_30` day-capping and `fixed_day(31)`→February month-end UTC rollover produce identical results on the catalog and Subscriptions sides, **and** a boundary fixture MUST assert that a mid-period **PriceWindow-split** proration produces identical results on the catalog↔Tariffs side
- **And** the three fields MUST be uniform across the recurring rows of one plan-`(currency, region)` — a divergent market fails publish (`PRORATION_CONTRACT_MIXED_MARKET`, D-123), so phase conversion never moves the anchor-derived cycle boundary and never changes the proration math

**62. Read-model monotonicity and staleness bound**
- **Given** an in-flight publish fan-out
- **When** a consumer resolves a price within the 5s warm window
- **Then** it MUST resolve against a single committed `CatalogVersion` and MUST NOT observe a partially-warmed version (the read model is **monotonic per `CatalogVersion`**; a version MUST be ignored until its `CatalogVersionPublished` **and** warm-completion marker — under batching a version bundles many `PlanPublished` events, so there is no per-`PlanPublished` marker)
- **And** a rating run MUST pin **one** `CatalogVersion` for its **entire duration** (determinism); at **pin time** the pinned version MUST NOT **lag the newest completed `CatalogVersion` by more than 5s** (a relative freshness bound, not an absolute age), then it holds for the whole run (no mid-run swap). This reconciles AC #51 and the event-propagation NFR without making long runs non-reproducible

**63. CatalogVersion increment on publish**
- **Given** a Plan passing validation and approval
- **When** `PlanPublished` completes
- **Then** the plan's content MUST become addressable in a `CatalogVersion`, with the registry as the **sole** incrementer; the registry **MAY batch** multiple approved publishes into **one** discretionary catalog publish (**not** one dedicated version per `PlanPublished`), and `pricingSnapshotRef` MUST pin the exact version containing the plan (a mass repricing MAY coalesce into one/few versions — AC #101)
- **And** the increment-trigger taxonomy and the max batching-delay SLO are **confirmed (D-47, 2026-07-28)** — interactive publish increments immediately (coalescing <= 5s), bulk coalesces with a 5-minute hard max, p95 pending→committed <= 60s — so publish-contract sign-off is no longer blocked on this item (see §15 Open Questions)

### Approval, governance, and access control

**64. Material-change comparison for multi-currency changes**
- **Given** a price change touching multiple `(currency, region)` rows
- **When** the approval-threshold policy is evaluated
- **Then** each affected row's delta MUST be compared against the per-currency threshold **in its own currency**
- **And** if **any** row exceeds its threshold, the two-person rule MUST apply

**65. ~~Historical-import (backdating) governance~~ — struck by D-330**
- **Struck 2026-08-16.** The criterion's requirement, `fr-historical-import-governance` (bare slug on purpose — see the struck FR's own entry), left the requirement set in the same wave, and the flow it tested (Slice 5 §2) left the design set. Nothing it asserted is owed: no restricted role, no mandatory reason, no row-shape parity subset, no disjoint reference store (D-76), no always-material import unit (D-13), no zero-billable-effect predicate
- **The number is kept rather than reused.** AC #5, #42 and #58 each cite "#65" and now cite it as struck; renumbering would silently rewrite three other criteria's meaning

**66. Cross-boundary mid-cycle change — operator warning**
- **Given** a mid-cycle change crossing currency, region, or billing frequency (modeled as cancel + new subscription per AC #46)
- **When** the operator previews or configures the change
- **Then** the preview/migration UI MUST warn that in-place proration credit is **forfeited** and the change is modeled as cancel + new subscription
- **And** the operator MUST explicitly confirm before proceeding
- **And** the **wording** of that warning is owned by the surface that renders it, not by the catalog (**D-169**, 2026-08-03): the read model publishes the machine-readable marker `crossBoundaryChangePolicy = cancel_plus_new` and **no** warning-text field, because a customer-visible string frozen into an INSERT-only version store could never be corrected or translated for any version already stamped with it. This AC is the normative home of the warning; Subscriptions countersigns it as the gear whose plan-change preview shows and confirms it

**67. Trial duration published for preview/quoting**
- **Given** a Plan with a `trial` phase declaring an authored trial length
- **When** published
- **Then** the catalog read model MUST publish `displayTrialDays` so preview/quoting can show the trial length (e.g. "14-day trial")
- **And** Subscriptions MUST enforce trial runtime **from this same published value** (single source: catalog authors, Subscriptions enforces) — `displayTrialDays` is not a second runtime authority

**68. Cross-region authoring authorization**
- **Given** a user whose IdP authorization region claim is `EU`
- **When** the user attempts to author or mutate a price row scoped to pricing `region=US`
- **Then** access MUST be denied unless the user's authz scope explicitly grants the target pricing region (pricing `region` is independent of, and not implied by, the authz region claim)
- **And** the attempt MUST be audit-logged (per AC #42)

### Discounts and approval governance

**69. Interim discount reference (conditional day-1 hook)**
- **Given** the Promotions PRD is not GA-ready and typed credit rows are Future scope
- **When** a Plan is published with an optional `discountRef`
- **Then** the catalog MUST validate that `discountRef` resolves to a registered external discount instrument and MUST persist it on the snapshot
- **And** the catalog MUST NOT author, evaluate, or stack the discount (evaluation owned by Tariffs/Promotions); absence of `discountRef` MUST NOT block publish

**70. Self-approval prohibition (segregation of duties)**
- **Given** a Plan submitted for publish that requires the two-person rule (material change, AC #28)
- **When** an approver acts on the submission
- **Then** the submitter's identity MUST NOT satisfy the required approval slot (the submitter and the independent approver MUST be **distinct principals**)
- **And** any self-approval attempt MUST be rejected and audit-logged with actor and timestamp

### Validation completeness

**71. Evaluation-policy fields forbidden on non-usage rows**
- **Given** a `flat` non-usage or `per_unit` price row
- **When** `tierAggregationWindow` or `billingGranularity` is present
- **Then** publish MUST fail with a field-level error
- **And** the aggregate fail-closed validation (AC #26) MUST include this condition

**72. Region taxonomy validation**
- **Given** a price row with a `region` value
- **When** the row is saved
- **Then** `region` MUST be a member of the tenant's configured region taxonomy
- **And** an unknown region MUST fail validation before publish

### Grandfathering and concurrency

**73. Grandfathering time-bound at renewal**
- **Given** a grandfathered price row with an optional `grandfatherUntil` (UTC)
- **When** a bound subscription renews on/after `grandfatherUntil`
- **Then** the published read model MUST signal that the subscription is no longer eligible for the grandfathered row
- **And** Subscriptions MUST re-bind it to the current eligible row at renewal (catalog publishes the bound; Subscriptions executes); a null `grandfatherUntil` means indefinite

**74. Concurrency precedence between bulk and interactive edits**
- **Given** an in-flight bulk price import holding per-row optimistic locks (AC #39)
- **When** a concurrent interactive plan-level edit (AC #37) targets a row under bulk lock
- **Then** the interactive edit MUST fail with a conflict naming the bulk operation
- **And** the system MUST NOT silently overwrite either change

### Scope-key and tax validation

**75. Brand taxonomy validation (brand-scoped `PriceOverlay`)**
- **Given** a **brand-scoped `PriceOverlay`** row with a `brand` value (`brand` is a `PriceOverlay` scope, **not** a price-row axis — see §2.2)
- **When** the row is saved
- **Then** `brand` MUST be a member of the tenant's configured brand taxonomy
- **And** an unknown/invalid brand MUST fail validation before publish (mirrors region validation, AC #72)

**76. Tax-exclusive without tax category**
- **Given** a price row with `taxInclusive=false` for a region with no configured `taxCategory`
- **When** publish is requested
- **Then** publication MUST warn or fail per the **tenant tax-display policy** (default fail-closed), symmetric with AC #19
- **And** the behaviour MUST be governed by the **same** tenant tax-display policy object (no separate undefined path)

### Lifecycle and structural integrity

**77. Published price rows are never deleted (supersede/retire only)**
- **Given** a **published** price row on the canonical scope key `(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)` — **whether or not** subscriptions are currently bound to it
- **When** removal/deletion of the row is attempted
- **Then** the system MUST reject the deletion — published rows are **append-only history** (AC #32, #97); there is **no deletion event** to fan out to warmed caches, and quotes, previews, exports, and `PriceOverlay` adjustments may reference the row
- **And** the operator MUST use supersession + grandfathering (or retirement + migration) instead, preserving the frozen snapshot for bound subscriptions; **deletion is available only for `draft` rows that were never published**

**78. Future-gap coverage across scheduled windows**
- **Given** a plan with two or more scheduled `PriceWindow` rows for one scope key
- **When** publish runs
- **Then** validation MUST reject any uncovered time interval (gap) between the end of one active/scheduled window and the start of the next for billable periods
- **And** MUST direct the operator to close the gap via the Slice 7 publish-time coverage / window-scheduling flow (extends the publish-time coverage check, AC #24)

**79. Add-on dependency acyclicity**
- **Given** add-on rules with inter-add-on dependencies
- **When** publish runs
- **Then** the system MUST reject any dependency cycle among add-ons (analogous to phase acyclicity, AC #54)
- **And** MUST report the cycle path

### Currency binding and lifecycle integrity

**80. Invoice currency binding**
- **Given** a subscription activating against a plan with multiple `(currency, region)` price rows
- **When** the subscription is created
- **Then** it MUST bind to exactly one `(currency, region)` row, and all recurring, usage, and add-on lines for that subscription MUST resolve in the bound currency
- **And** publish/preview MUST reject the **enumerated** configurations that would force mixed-currency lines onto a single invoice: (i) a **required** add-on or an add-on **price-override target** lacking a row in a currency the base plan publishes — an **optional** add-on's currency gap is enforced at **attachment** time instead and MUST NOT block the base plan's publish (D-73); (ii) a `sum_of_parts` bundle whose component rows do not cover every currency the bundle sells; (iii) an `own_price` bundle without a matching-currency component set. (currency selection at activation is owned by Subscriptions; the single-currency-per-invoice invariant is enforced here)

**81. Clone field semantics**
- **Given** a clone of a source Plan (AC #38)
- **When** the new draft is created
- **Then** `priceEligibility` and `grandfatherUntil` MUST reset to defaults (not copied), and contract locks MUST NOT be copied
- **And** `discountRef` MUST be copied only if it still resolves to a registered instrument (else dropped with an operator notice); the clone MUST record `clonedFrom` and MUST NOT bind to any source subscription

**82. Upstream SKU retirement coherence**
- **Given** a registry SKU referenced by draft or published plans
- **When** the registry retires/unpublishes that SKU
- **Then** new plan publishes against it MUST be blocked, affected drafts MUST be surfaced to operators, and posted snapshots and active-subscription bindings MUST NOT be mutated
- **And** the catalog MUST emit a signal enabling operators to remediate affected drafts (contract coordinated with the registry)

### Authoring guardrails and discount GA gate

**83. Rev-share residual guardrails**
- **Given** a bundle with rev-share where a non-platform party is nominated primary (AC #23)
- **When** the rounding residual is absorbed
- **Then** no party's resolved share may be < 0%, and the residual MUST be absorbed per `(bundle, vendor SKU)` independently within <= 0.01% tolerance
- **And** publish MUST fail if reconciliation would force any negative share

**84. `customEveryN Days` anchor compatibility**
- **Given** a recurring plan with `frequency=customEveryN Days(n)` (AC #45)
- **When** `billingAnchorPolicy` is `calendar_month` or `fixed_day(d)`
- **Then** publish MUST fail — a `Days`-based custom cycle MUST anchor on `subscription_start`
- **And** `n` MUST NOT exceed the configured custom-interval cap (tied to the size-limit NFR, AC #104)

**85. `PriceOverlay` adjustment tax basis**
- **Given** a `PriceOverlay` markup/discount/fixed adjustment authored against a base price row (AC #55)
- **When** the base row is `taxInclusive=true`
- **Then** the adjustment row MUST declare the basis it applies to (inclusive vs exclusive) or explicitly delegate the basis to Tariffs
- **And** publish MUST reject a `PriceOverlay` adjustment that leaves the basis ambiguous

**86. Refund/cancellation credit basis (conditional)**
- **Given** a recurring price row eligible for cancellation credit — **conditional: this AC activates only on the Finance day-1 decision; otherwise the field remains Future scope**
- **When** published
- **Then** the read model MUST expose a `cancellationCreditBasis` — a **closed** enum (`daily` \| `none`) frozen in `pricingSnapshotRef`, with credit execution owned by Billing/Payments (extending the enum requires a versioned contract change, not an open-ended value set)
- **And** absence MUST mean "no cancellation credit" (fail-safe), **not** undefined

**87. Discount-path availability at sellable-GA**
- **Given** a sellable-GA readiness review
- **When** a Plan is assessed for sale with discounts
- **Then** at least one supported discount path MUST be available (approved Promotions PRD, typed credit row, or `discountRef` in committed scope)
- **And** absent all paths, the Plan MAY publish but MUST be flagged **not-sellable-GA** (a **plan-level** GA flag covering every scope key of the plan — distinct from the per-market tax-inclusive flag of AC #18; per the §13 External dependencies GA gate)
- **And** `discountRef` is **committed to launch scope** (decided 2026-07-04) — the discount path is satisfied at MVP; a full Promotions PRD remains Future

### Package and prepaid pricing

**88. Package (block) pricing**
- **Given** a usage price row with `modelKind=package`, `packageSize`, and `packagePrice`
- **When** the plan is published
- **Then** the read model MUST expose `packageSize`, `packagePrice`, and the row's `tierAggregationWindow` (D-58) with no tier-band fields
- **And** Tariffs MUST bill by rounding usage **up** to whole blocks **over that window** (blocks = ceil(used-in-window / `packageSize`), charge = blocks × `packagePrice`); catalog MUST NOT compute the charge
- **And** this repeating-block formula MUST be reconciled in §17.2 and gated on a joint golden fixture (AC #60) — it is **distinct** from Tariffs' Volume Variant B (per-tier flat fee)

**89. Package pricing validation**
- **Given** a `modelKind=package` price row
- **When** `packageSize <= 0`, `packagePrice < 0`, any tier-band field is present, **or `tierAggregationWindow` is unset** (D-58)
- **Then** publish MUST fail with a field-level error (`PACKAGE_FIELDS_INVALID`; `EVAL_POLICY_MISSING` for the missing window)
- **And** the aggregate fail-closed validation (AC #26) MUST include these conditions

**90. Prepaid credit grant definition**
- **Given** a plan declaring a prepaid credit grant (`grantAmount`, `creditUnit`, `category`, `price`, `expiryPolicy`, `autoRechargeAllowed`, `applicability`, `drawdownPriority`)
- **When** the plan is published
- **Then** the grant fields MUST persist and be frozen into `pricingSnapshotRef`, with the resolved `applicability` **materialized** (authored `all_usage` resolves to the plan's usage lines; a metered `creditUnit` bounds it to that unit's meters) — the executor never infers scope (D-43)
- **And** for `category = prepaid` the grant `price` MUST be scoped per `(currency, region)` like price rows; an unscoped grant price MUST fail publish for a plan selling in multiple `(currency, region)`, the grant-price set MUST cover **every** `(currency, region)` the plan publishes sellable rows for (a missing market fails publish), and grant-price changes are subject to the same approval-threshold / material-change policy (AC #28, #64)
- **And** for `category = promotional` the grant is issued free: price rows and `autoRechargeAllowed` MUST be absent (publish fails otherwise); `expiryPolicy = never` warns
- **And** an `applicability` entry that is unpublished or not a usage line of the plan MUST fail publish; `drawdownPriority` is an authored default — the effective cross-grant order is Billing-owned (`drawdownPriority` → `promotional` first → earlier expiry → earlier issuance → `grantId`, D-43)
- **And** the catalog MUST NOT track balance, compute drawdown, or order live balances (owned by Billing/Rating)

**90a. Included allowance (D-45)**
- **Given** a `usage` row declaring `includedAllowance = { quantity N, rolloverPolicy }`
- **When** the plan is published
- **Then** for `rolloverPolicy = none` publish MUST compile the declaration into the `$0` first band `[0, N)` plus a frozen first-class allowance marker, and the resolved amounts MUST equal the equivalent hand-authored $0-band row exactly (compile-equivalence)
- **And** compile-equivalence is a property of every row that **publishes**, not of the compile in the abstract (qualified 2026-08-15, D-317): offsetting the authored bounds by `+N` saturates at the integer ceiling, and a saturated offset yields a band set that passes every band rule while being materially narrower than authored — so publish MUST refuse such a row (`ALLOWANCE_QUANTITY_INVALID`) rather than freeze a projection that is not equivalent to what its author wrote
- **And** for `rolloverPolicy = carry(maxPeriods)` publish MUST materialize the per-period promotional grant (free, `applicability` = this row's meter, expiry = the carry horizon) under the D-43 rules; drawdown execution stays Billing-owned
- **And** publish MUST fail on: a non-`usage` row; a row combining an authored `$0` first band with `includedAllowance`; a non-`sum` row (`aggregationFunction ≠ sum`); an unusable `quantity` (`≤ 0`, or one whose offset saturates the authored ladder); a `package`, `volume` or `flat` row (`ALLOWANCE_KIND_UNSUPPORTED`); a row carrying reservation attributes (`ALLOWANCE_WITH_RESERVATION`)

**91. Prepaid grant expiry validation**
- **Given** a prepaid credit grant
- **When** `expiryPolicy` is unset or set to `days(N)` with N <= 0
- **Then** publish MUST fail (no implicit "never expires")
- **And** only `never` and `days(N>0)` are valid values

**92. Prepaid grant credit-unit integrity**
- **Given** a prepaid credit grant whose `creditUnit` is a `meteringUnit`
- **When** that `meteringUnit` is not published
- **Then** publish MUST fail
- **And** the catalog MUST NOT persist or expose any prepaid balance (balance ledger owned by Billing/Rating)

### Non-Functional Requirements (Show-Stoppers)

**93. Plan mutation latency**
- **Given** authorized operators
- **When** plan create or update is submitted
- **Then** end-to-end persistence and validation MUST complete within 2 seconds at p95

**94. Price field validation latency**
- **Given** a single price row save
- **When** currency/region/tier validation runs
- **Then** validation MUST complete within 200ms at p95

**95. Publish propagation**
- **Given** a committed `CatalogVersionPublished` event (the point at which a batched publish becomes addressable)
- **When** Rating cache warming runs
- **Then** plan definitions MUST be visible within 5 seconds at p95 of `CatalogVersionPublished`
- **And** the registry MUST bound the delay from `PlanPublished` (pending) to `CatalogVersionPublished` (committed) by the **max batching-delay SLO** (D-47: interactive <= 5s coalescing; bulk <= 5 min hard max; p95 pending→committed <= 60s)

**96. Plan read latency**
- **Given** catalog browse or plan detail read
- **When** served from read model within tenant partition
- **Then** p95 latency MUST be under 100ms

**97. Audit completeness and tamper-evidence**
- **Given** any plan or price mutation
- **When** the operation completes
- **Then** the system MUST record actor, timestamp, before/after version, and approval trail
- **And** MUST retain price history for **>= 7 years**, with the retention period **tenant/jurisdiction-configurable**
- **And** where multiple jurisdictional minimums apply, retention MUST be the **maximum** applicable minimum per the data row's residency jurisdiction(s)
- **And** audit and price-history records MUST be stored **append-only / tamper-evident** (hash-chained in the transactional store; optionally anchored to external WORM) so prior versions cannot be mutated or deleted within the retention window
- **And** where a jurisdiction imposes a retention *maximum* (storage-limitation regime) that conflicts with the max-minimum rule, Legal MUST resolve the conflict per the row's residency jurisdiction (see §15 Open Questions)

**98. Event propagation**
- **Given** `PlanCreated` or `PlanUpdated`
- **When** emitted after mutation
- **Then** downstream consumers MUST receive event within 5 seconds at p95
- **And** events MUST include correlation/idempotency keys per manifest governance
- **And** consumers MUST be able to dedupe by idempotency key (at-least-once delivery assumed; duplicate redelivery MUST be safe)

**99. Multi-currency scale**
- **Given** a Plan with many currency rows
- **When** stored and read
- **Then** the system MUST support **at least 20 currencies per plan as a guaranteed floor** (tested at 20; not merely a benchmark point)
- **And** the read SLO (p95 < 100ms, AC #96) MUST hold **at the 20-currency floor**, not only at nominal load

**100. Mutation API idempotency**
- **Given** a plan or price create/update call carrying a client idempotency key
- **When** the same key is retried
- **Then** the system MUST return the original result **once the original has completed**, and MUST NOT create a duplicate draft plan or price row
- **And** the key MUST be honoured for a **24-hour TTL** (ratified 2026-07-28; the Stripe-convention retention window, documented in the API contract); a key reused after the TTL MAY be treated as a new request
- **And** an idempotency-key **replay during an active bulk lock** (AC #74) MUST return the original completed result regardless of current lock state (it is a read of a completed op); a **new** key targeting a locked row follows AC #74
- **And** the TTL MUST be evaluated when the key is **claimed** and **before** the retried payload is compared against the stored request digest (**D-142**, 2026-08-02 implementation finding) — with the comparison first, the first payload to touch a key owns it past 24h and the ratified TTL bounds nothing; and an answer once recorded MUST be written **once**, so a retry after completion returns the **stored** response rather than a re-executed or overwritten one
- **And** a duplicate arriving **while the original is still in flight** — reachable with no contract violation by either caller — MUST be answered `IDEMPOTENCY_KEY_IN_FLIGHT` (409) naming retry as the remedy, never a not-found refusal on a request that both exists and is succeeding, and never a 500 (**D-143**, 2026-08-02 implementation finding — flagged for veto; it is what narrows the "original result on retry" MUST above)

**101. Mass repricing performance and idempotency**
- **Given** a mass price adjustment over N rows (e.g. an annual increase across many plans)
- **When** executed or re-executed after a partial failure
- **Then** the operation MUST be idempotent (re-run-safe) and MUST emit deduplicated events
- **And** the throughput SLO MUST be back-calculated from the tenant worst-case row count (plans × currencies × regions) against an agreed maintenance window (>= 50 rows/sec — ratified 2026-07-28 as the launch default; the perf test verifies it against the worst case, an engineering validation, not a re-decision)

**102. Data residency for price and audit data**
- **Given** a tenant with a jurisdiction-bound residency requirement
- **When** price and audit/history data are stored and replicated
- **Then** the system MUST keep that data within the configured jurisdiction's residency boundary and MUST NOT replicate a row outside **any** of its residency jurisdictions' boundaries
- **And** residency MUST be consistent with the jurisdiction-configurable retention in the audit-completeness NFR (AC #97)

**103. Read-model availability and disaster recovery**
- **Given** Rating's hard real-time dependency on the published read model
- **When** the read model serves rating/preview traffic
- **Then** it MUST meet a stated availability SLO (target >= 99.9% per tenant partition) and a disaster-recovery **RPO <= 5 min** / **RTO <= 30 min** (ratified 2026-07-28 as launch targets)
- **And** during a read-model outage, charge resolution MUST **NOT resolve from an unavailable or partially-warmed read model** and MUST fail closed; **frozen `pricingSnapshotRef` resolution is unaffected** (rating against an intentionally frozen snapshot is the core mechanism, not "stale" rating), consistent with AC #62

**104. Plan and tier size limits**
- **Given** a Plan with many tier bands and/or price rows
- **When** authored or published
- **Then** the system SHOULD enforce configurable soft caps on tier-band count per row and price-row count per plan, emitting a publish warning above the cap (<= 100 tier bands per row, <= 500 price rows per plan — ratified 2026-07-28, tenant-configurable), and MUST enforce the custom-interval cap of AC #84 (`customEveryNDays` <= 366, `customEveryNMonths` <= 24 — ratified 2026-07-28, tenant-configurable)
- **And** the caps MUST protect the read SLO (AC #96) and mass-repricing SLO (AC #101) from pathological plans

### Commercial-shape and lifecycle completeness (review follow-ups)

**105. Recurring billing timing (`billingTiming`)**
- **Given** a recurring or hybrid Plan submitted for publish
- **When** validation runs
- **Then** every recurring price row MUST carry `billingTiming` (`in_advance` \| `in_arrears`), frozen in `pricingSnapshotRef`; usage rows are implicitly `in_arrears`
- **And** publish MUST fail if a recurring row omits `billingTiming`; a hybrid plan MAY combine an `in_advance` base row with `in_arrears` usage rows (Billing derives its deferral policy from this — Billing PRD)

**106. Optional one-time setup/activation charge on recurring/hybrid plans**
- **Given** a recurring or hybrid Plan with an optional one-time setup price row on the same `planId`
- **When** the Plan is published
- **Then** the read model MUST expose the setup row as a **first-class** plan price row (in approvals, `pricingSnapshotRef`, and preview), charged **once per subscription lifetime** (at activation; at entry into the first non-trial phase for trialed plans; never re-charged on plan change/`PlanLink` migration)
- **And** publish MUST validate it as one-time (no recurrence, no `billingTiming`/tier fields) — it MUST NOT require a synthetic add-on SKU

**107. Purchase gated on an active window (sellability)**
- **Given** a published Plan whose bound canonical scope key has only a **scheduled** (not-yet-active) `PriceWindow` at time `t`
- **When** subscription/purchase creation is attempted at `t`
- **Then** creation MUST be blocked (joint rule with Subscriptions) — a plan MUST NOT be sold before an active window covers the bound scope key, preventing a first rating that resolves no window (fails closed)
- **And** optional plan-level `availableFrom`/`availableTo` (the unified purchasability-dating pair for **all** billing cycles), when set, MUST be validated against window coverage at publish; deferred publish ("publish at T") is out of launch scope

**108. Plan-change contract published**
- **Given** a Plan that participates in self-service plan changes
- **When** the Plan is published
- **Then** the read model MUST publish `allowedChangeTargets` (**explicit** target `planId`s; rules are Future scope) and a `comparabilityRank` — REQUIRED for every participating plan, `PlanTier` is never an ordering at launch (§17.8) — so Subscriptions can classify upgrade/downgrade/switch and constrain targets
- **And** every edge's boundary classification (`in_place` | `cancel_plus_new`) MUST be computable at change time from both plans' published market/frequency facts at the pinned version (D-93 — never a publish-time stamp) so cross-boundary consequences are disclosed before execution; a retired target makes the edge inert (re-checked at change time)
- **And** absence of `allowedChangeTargets` MUST mean **no self-service change** (fail-safe), not any-to-any; enforcement remains in Subscriptions

**109. Customer-group segment pricing**
- **Given** an operator-defined customer group (BSS-owned taxonomy) and a `customerGroup`-scoped `PriceOverlay` carrying a **line set** (D-42) — each line keyed `(planId?, targetSku?)` with its own `markup`/`discount`/`fixed` magnitude; lines carry **no region axis** (region variance comes from the region-scoped base rows a line applies to)
- **When** the plan is published and a payer's effective-dated group membership is resolved via `payerTenantId`
- **Then** the read model MUST expose the `customerGroup` `PriceOverlay` and the group taxonomy; Tariffs resolves the payer's group and applies the overlay (§17.7); the **resolved group** MUST be frozen in `pricingSnapshotRef`
- **And** a membership change is **renewal-aligned by default** (resolved group pinned in the subscription snapshot until renewal; immediate re-resolution is an explicit material change), a group discount/move affecting many payers is a **material change** (two-person rule, AC #28), and all membership changes MUST be audited
- **And** a payer holds **at most one active membership across all groups** — a conflicting enrollment MUST be rejected (409) naming the active membership; a transfer MUST be one atomic audited move (end + start, no gap or overlap instant)
- **And** entirely different **tier structures** per group are **out of launch scope** (use separate plans + group-scoped plan eligibility — Future)

### Governance and referential-integrity completeness (review follow-ups)

**110. Pending-approval content pin**
- **Given** a material change submitted for approval (content hash pinned per AC #28 mechanics)
- **When** the subject is mutated while the approval is pending
- **Then** the approval MUST be voided (back to draft; a fresh submit is required)
- **And** an approval decision MUST verify the pinned content hash and reject on mismatch — a reviewer approves exactly what they reviewed
- **And** an approver whose authorization scope does not cover every region/brand the pinned change touches MUST be rejected (403) and the attempt audited

**111. Bulk approval per-row pin and reuse**
- **Given** a material bulk batch approved with per-row content hashes
- **When** Phase-2 per-row commits conflict and shrink the committed set
- **Then** the committed set MUST be a subset of the approved set
- **And** a retry of conflicted rows with unchanged content MUST reuse the original approval; a changed row MUST require a fresh approval

**112. Single pending approval unit per scope key**
- **Given** a pending approval unit **of any kind** holding a canonical scope key (supersession, grandfathering cutover, window mutation, retirement, bulk batch — any unit whose change set touches the key)
- **When** a second unit touching the same key is submitted
- **Then** the submission MUST be rejected with a conflict (409) naming the pending unit
- **And** the rule is symmetric with bulk operations: a bulk/repricing row whose key holds a pending interactive unit fails per-row, and a submitted material batch pins its keys against interactive submits (409 naming the bulk operation)

**113. Retirement referential guard**
- **Given** a plan referenced as a bundle component or as an add-on price-override target
- **When** retirement is requested
- **Then** the request MUST be rejected with the referencing compositions enumerated; remediation precedes retirement

**114. PriceOverlay disclosure default**
- **Given** a `PriceOverlay` authored without an explicit `disclosure` value
- **When** it is saved/published
- **Then** `disclosure` MUST default to `restricted` (fail-closed)
- **And** consumer-facing enumeration/preview MUST NOT expose a restricted overlay (or its existence) to out-of-scope payers

**115. Cross-class tie-break published**
- **Given** `PriceOverlays` of different scope classes with overlapping targets
- **When** the read model is published
- **Then** it MUST expose the normative class-specificity tie-break `customerGroup > partner > orgTier > brand > region > global`
- **And** Tariffs MUST adopt the tie-break verbatim (joint fixture; AC #61 mechanics)

### Instant precision and grandfathering-field placement (implementation-finding follow-ups)

**116. Instant precision on authored dates**
- **Given** an authored instant on a price row, `PriceWindow`, cutover, or plan availability pair — `effectiveFrom`/`effectiveTo`, the D-88 changeover instant, a cutover instant, `grandfatherUntil`, `availableFrom`/`availableTo`
- **When** it carries finer-than-millisecond precision
- **Then** the request MUST be rejected (`TIMESTAMP_PRECISION_EXCEEDED`, 422) and the value MUST NOT be silently truncated (**D-144**, 2026-08-02 implementation finding)
- **And** every instant the catalog publishes or compares MUST be **UTC at millisecond resolution**, the `cohort` axis included — the axis is matched for **equality** against a window bound produced in another gear (Rating's D-126 bootstrap), so an unquantized cohort makes a grandfathered generation unfindable by the very subscribers it protects, with no failure anywhere in between
- **And** storage bookkeeping no operator authors (`created_at`, audit-chain and outbox timestamps) is outside the rule — it enters no contract field and is compared with nothing

**117. `grandfatherUntil` confined to grandfathered rows**
- **Given** a price row carrying a non-null `grandfatherUntil`
- **When** publish validation runs
- **Then** publish MUST fail (`GRANDFATHER_UNTIL_FORBIDDEN`, 422) unless the row's `priceEligibility = existing_grandfathered` (**D-147**, 2026-08-02 implementation finding) — the pairing was enforced only by a physical column check, so the operator received an internal fault instead of a refusal naming the field they could correct
- **And** the bound MUST NOT be read as a general per-row availability date: "this row stops being sellable at `T`" is owned by `PriceWindow` `effectiveTo` and the plan's `availableTo`, and only the eligibility machinery derives a signal from `grandfatherUntil` (re-bind at the next renewal, AC #73)
- **And** a grandfathered row with a null `grandfatherUntil` remains indefinite — the converse is deliberately not constrained

## 13. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| Catalog registry (Product & SKU) | Published `skuId`, `bundle` SKU type, `meteringUnit` declaration, `PlanTier` value/taxonomy, tax/GL codes, and `CatalogVersion` (sole incrementer); increment taxonomy + batching SLO **ratified (D-47, 2026-07-28)** | `p1` |
| ~~PriceWindow (effective-dating use case)~~ | **Consolidated into this PRD** (§15 answered): window scheduling/activation/events owned by the pricing gear; the legacy UC doc is scenario source material — no external dependency remains | — |
| Subscriptions | Owns the plan-change boundary/mode + runtime, plan-change classification, trial runtime, entitlement enforcement, `PlanLink` migration, sellability checks from published inputs (proration math = rating gear); proration seam is a GA gate | `p1` |
| Rating (incl. the evaluation core — former "Tariffs / PLAL"; §3.2) | Consumes events + warmed read models; resolves deterministic inputs; owns Usage -> RatedCharge orchestration. Its **evaluation core** consumes the read model (model kind, tiers, evaluation policy, reserved rate, derived-meter definition, `customerGroup` overlays), evaluates formulas/overlays/FX, composes `pricingSnapshotRef`, and adopts the ten-axis scope key — resolving a grandfathered generation by the pinned price id's `cohort` (ADR-0002). One counterpart gear, not two (2026-07-28 review fix) | `p1` |
| Billing / Payments | Consumes descriptors via `CatalogVersion`; derives deferral policy from `billingTiming`; owns refunds/credits and PSP/ERP posting; descriptor v1 field set pinned (D-48: invoice line template/labels, `taxCategory`, `glCode`, itemization rule, `billingTiming`) — Billing countersigns at its gear PRD | `p1` |
| Tax Engine | Tax-scheme determination + `region` -> jurisdiction mapping; **confirmed post-MVP** (ETA ~8 months) — MVP is tax-exclusive; tax-inclusive plans GA-gated | `p1` |
| Contracts & Agreements | Contract locks (exclude locked subscriptions from migration); negotiated RI-style reservation rates | `p1` |
| Promotions / Coupons | Coupon/discount authoring + evaluation (dedicated PRD **TBD — does not yet exist**); `discountRef` resolves to a registered external instrument | `p2` |
| Marketplace | Consumes bundle rev-share rules for fee accrual | `p2` |
| Prepaid balance ledger + drawdown + auto-recharge | Billing / Rating; **TBD** — prepaid grants can be defined but not sold/consumed until balance execution exists | `p2` |
| BSS Architecture Manifest | §4.1 Catalog, §4.2 Rating, §4.3 Subscriptions, §2.1.3 identities, §4.8 Marketplace | `p1` |

> **Conformance fixtures before code**: the jointly-owned golden fixtures (tier-boundary per AC #60, proration per AC #61, level-aggregation per D-44 — granule fold, late-sample re-fold, `maxHold` gap) MUST be stood up and version-controlled in a shared repo **before** implementation; publish-contract sign-off is gated on green fixtures. Every GA-gate dependency MUST be tracked on the program board with a named owner and target date.

## 14. Assumptions

- NFR targets (read-model availability/DR RPO/RTO 99.9%/5m/30m, mass-repricing >= 50 rows/s, plan/tier size caps 100/500 + interval caps 366d/24m, idempotency-key TTL 24h) — **ratified 2026-07-28 by the product owner as launch defaults**; the mass-repricing figure is perf-test-verified against the tenant worst case. The four caps called **tenant-configurable** (100 bands/row, 500 rows/plan, 366 days, 24 months) are declared per tenant in `pricing_policy_object`, a tenant with no entry taking the ratified default (**D-152**, 2026-08-03 — the adjective had named no store, and the gear's configuration section is per deployment). **Background-sweep cadences are ratified here too, and each names the bound it derives from rather than being chosen** — the read-model warm re-drive at **5s** from §1.2's publish→read-model SLO, the `CatalogVersion` overdue threshold at **300s** from D-47's max batching delay, window activation at **60s** as one order inside D-47's five-minute authoring floor, and the **gated-market gauge at 60s** (**D-250**, 2026-08-07): its subject — a market gated on tax-category readiness — moves when an operator declares a category or the post-MVP Tax Engine answers, which §17's risk table sizes in months, so the tick is conservative by many orders and is **not to be tightened**, the refresher's read being a catalog-wide scan rather than an index probe.
- The catalog registry is SoR for Product/SKU/Category/Attribute/`CatalogVersion`, the `bundle` SKU type, the metering-unit declaration, and the `PlanTier` taxonomy; this PRD composes **published** units and **freezes** plan/price/descriptor content into the `CatalogVersion` the registry commits.
- `CatalogVersion` uses a **batched/discretionary** publish model: `PlanPublished` carries a pending ref and `pricingSnapshotRef` pins the committed batch version; the increment-trigger taxonomy (incl. the overlay/membership publish units, §17.5) and the SLO are ratified (D-47: interactive <= 5s coalescing; bulk <= 5 min hard max; p95 pending→committed <= 60s) — D-47, mirrored registry-side.
- Tax Engine is confirmed **post-MVP**; MVP sells **tax-exclusive** and `taxInclusive=true` **price rows/markets** are authorable but flagged **not-sellable-GA** (per `(currency, region)` market, not the plan as a whole) until Tax Engine GA.
- `discountRef` is committed as the **day-1 discount hook** (referential-integrity only); a full Promotions PRD remains the durable owner (Future).
- Subscriptions owns the change boundary/runtime/`PlanLink`; Tariffs evaluates formulas, overlays, FX, and proration; Billing posts invoices and derives deferral policy from `billingTiming`; the catalog computes **no** monetary charges.
- PriceWindow scheduling, the UTC activation job, and `PriceWindow*` event emission are owned by the pricing gear (Slice 7, D-03 / ADR-0003); the legacy effective-dating price-windows use case is retained only as a scenario source, not a runtime owner. This PRD owns linkage, coverage, and activation.
- Prepaid credit **balance execution** (ledger, drawdown, zero cut-off, auto-recharge) is owned by Billing/Rating; the catalog defines the grant only.

## 15. Open Questions

> Decisions already resolved for this PRD are recorded inline (Glossary, §17 appendices, Acceptance Criteria). This table lists only items still requiring an external decision or sign-off.

| **Question** | **Status / Owner** | **Date Answered** |
|--------------|--------------------|-------------------|
| Day-1 discount: ratify a Promotions PRD (owner + date) **or** commit `discountRef` into scope? | **Answered — `discountRef` committed into launch Scope** (day-1 discount hook; a full Promotions PRD remains the durable owner, Future). Owner: Program + GTM. | 2026-07-04 |
| Tax Engine PRD exists (scheme determination + `region`->jurisdiction mapping)? | **Answered — confirmed but post-MVP** (roadmap P1 right after MVP, ETA ~8 months). MVP is **tax-exclusive**; `taxInclusive=true` is **authorable but a GA gate** (AC #18/#19). Owner: Architecture/Tax. | 2026-07-04 |
| `CatalogVersion` increment trigger taxonomy (per-publish vs **batched/discretionary**; mass-repricing coalescing; **max batching-delay SLO** per AC #95); registry as sole incrementer? | **Answered — closed with the (vendored) registry, D-47**: trigger taxonomy = **interactive** publish increments immediately (coalescing window <= 5s), **bulk** (mass repricing, migrations, bulk enrollments) coalesces into one version with a **5-minute hard max delay**; SLO = **p95 pending→committed <= 60s, max 5 min** (the number AC #95 and the propagation alarms bind to). Registry stays the sole incrementer. Mirrored in the products-gear PRD. | 2026-07-28 |
| Upstream SKU retirement/unpublish joint contract while plans reference it? | **Answered — joint contract closed by cross-reference (D-47)**: the registry never retires a SKU with active plan references (its `SkuReferenceCount` 3-state predicate, fail-closed without a fresh watermark — products PRD `fr-sku-lifecycle` side); pricing flags referencing plans on the registry's deprecation signal and blocks new adoption (AC #82 side). Both PRDs now cite each other's half. | 2026-07-28 |
| Minimum Billing/ERP descriptor field set + PSP/ERP posting contract beyond `glCode` + events? | **Answered — catalog-side v1 pinned (D-48)**: the descriptor set = **invoice line template/labels, `taxCategory`, `glCode`, itemization rule, `billingTiming`** — sufficient to render the line and post to GL with no catalog re-query (AC #53); extension is **additive only**. Status: tracked dependency — Billing countersigns when its gear PRD lands. | 2026-07-28 |
| Cancellation/refund credits required at launch? | **Answered — no credits at launch (product owner, D-49)**: self-service cancel defaults `end_of_term` (no refund question); an immediate cancel forfeits the remainder, exceptions are operator-handled Billing-side. AC #86 stays dormant; activating it later is the recorded reopen path. `creditOnDowngrade` (plan change) is unaffected. | 2026-07-28 |
| Cross-boundary (currency/region/frequency) cancel+new limitation — written sign-off + customer-facing constraint entry? | **Answered — signed off (product owner, D-49)**: the limitation stands (in-place cross-currency/region/frequency change unsupported; the sequenced cancel+new saga is the path — subscriptions design/03 §4.3); GTM/customer-facing docs MUST carry the constraint entry ("changing country/currency = a new subscription"). | 2026-07-28 |
| Two-dimensional (seats × usage) single-line pricing needed at launch? | **Answered — no, stays Future (product owner, D-49)**: the launch product set (infra: cloudlets, storage) has no seat-scaling; the two-row workaround (per_unit recurring + usage) covers seats + a flat included pool. Note the honest boundary: a **seat-scaled** allowance is not expressible — that is exactly the named D-45 Future gate, the reopen path if such a product appears. | 2026-07-28 |
| Freemium: per-row `$0` vs a first-class structural flag at launch? | **Answered — no structural flag at launch (product owner, D-49)**: trials are phases (structurally distinct), the D-45 allowance marker separates included-free from promo `$0` bands, and a freemium **plan** is declared via the `PlanTier` taxonomy (a `free` tier) / derivable from plan shape (all rows $0, no paid phase). Reopen only if Analytics demonstrably cannot distinguish — the flag is then an additive column. | 2026-07-28 |
| Provisional NFR values to ratify before Design (no bare placeholders may ship): read-model availability/DR RPO/RTO (AC #103), mass-repricing worst-case throughput (AC #101), plan/tier size caps + custom-interval cap (AC #104/#84), idempotency-key TTL (AC #100)? | **Answered — ratified as launch defaults (product owner)**: 99.9% / RPO 5m / RTO 30m; >= 50 rows/s (perf-test-verified vs worst case); 100 bands/row + 500 rows/plan (soft) + 366d/24m interval caps; idempotency TTL **24h**. | 2026-07-28 |
| Retention "max applicable minimum" vs jurisdictions imposing a retention *maximum* (storage-limitation)? | **Open** — Legal to resolve per residency jurisdiction (AC #97). Owner: Legal. | — |
| Rate-limiting / per-tenant mutation quotas (abuse control)? | **Deferred** — out of this PRD's scope but MUST land as a platform/gateway NFR. Owner: Engineering/Platform. | — |
| Minimum customer notice for enforced migration (business policy)? | **Answered — configurable per tenant, default floor 60 days (product owner, D-49)**: scheduling an enforced migration validates `effectiveAt >= announcement (PlanMigrationScheduled) + configured notice`; tenants may raise the floor, never silently lower it below the configured value. | 2026-07-28 |
| SSP / ASC 606 tags on plan publish? | **Deferred** to Future scope / Finance workshop. Owner: Finance. | — |
| Consolidate the effective-dating price-windows use case into a standalone PRD? | **Answered — consolidated INTO this PRD (2026-07-10, D-03)**: the pricing gear owns the window store, state machine, activation job, and `PriceWindow*` emission (frozen manifest names); no standalone PRD. The UC doc is scenario source material; its FX-rate-lock and subscription-impact-preview scenarios are dispositioned out (no FX in catalog; impact preview needs Subscriptions data). Formal Architecture ack pending. Owner: Architecture. | 2026-07-10 |
| Volume **Variant B** (per-tier block fee) — needed at launch or dropped from Tariffs? (F-39) | **Answered — dropped from Tariffs** (not authorable from the catalog; catalog `volume` = Variant A only). Owner: Product + Tariffs. | 2026-07-04 |
| Self-service **reserved-capacity rate** a launch/migration requirement (§17.7)? (F-42) | **Answered — yes, in launch Scope** (predecessor parity). Tariffs sources the reserved rate from the catalog snapshot; negotiated RI-style rates stay in Contracts. Owner: Product + Tariffs + Contracts. | 2026-07-04 |
| Peak / period-end billing — authorable **`aggregationFunction`** (`sum`\|`max`\|`last`\|`unique`) / capacity-peak kind a launch case? (F-40) | **RE-DECIDED — yes, in launch (2026-07-16, D-44)**, superseding the 2026-07-04 "no": the launch product set bills on levels (cloudlet peak-per-hour, storage GB-month). `aggregationFunction ∈ {sum, peak, time_weighted}` + `aggregationGranularity {hour, day}`; granule-fold summed into an **additive** `Q` (Glossary); frozen in `pricingSnapshotRef`; `last`/`unique` stay Future. Rating semantics: rating T-D-17 + joint fixture. Owner: Product + Rating. | 2026-07-16 |
| First-class **`includedAllowance`** (+ `rolloverPolicy`) vs the `$0`-first-band representation at launch? (F-32) | **Answered — no** (2026-07-04). **RE-DECIDED — yes, in launch (2026-07-16, D-45)**: existing-SKU migration needs first-class allowance. Authored `includedAllowance {quantity, rolloverPolicy {none, carry}}`, publish-**compiled** (`none` → $0 band + frozen marker; `carry` → D-43 per-period promotional grant, Billing executes); `sum` rows only; per-seat and level-meter allowance stay Future. Owner: Product + Analytics. | 2026-07-16 |
| **Customer-group membership subtree inheritance** (design/09 L7): a parent-payer membership covering payer-subtenants via a hierarchy walk (nearest-wins, `inherited_via` frozen, cascade materiality) vs per-subtenant enrollment (the launch behavior — direct `payerTenantId` only)? | **Open** (latent — per-subtenant enrollment works at launch, automatable off AMS subtenant events; registered here 2026-07-28, previously slice-prose-only). Owner: Product + Architecture. | — |
| **Eligibility-gate policy set** (design/09 `inst-cg-eligibility-policy`; gates F-88 `eligibleCustomerGroups` activation): membership-ends-mid-subscription semantics; operator sale outside the group (enroll-first vs override); interaction with `allowedChangeTargets`? | **Open** (latent — nothing activates until F-88 reopens; registered here 2026-07-28, previously slice-prose-only). Owner: Product + Finance. | — |
| Tariffs to add a **partner-facing effective-price preview** (with hierarchy) so the migrated use-case promise has an owner? (F-34) | **Open — registered as a tracked GA gate on the program board** (owner: Tariffs + GTM): restricted segment pricing does not sell **self-service** until F-34 lands; nothing else holds on it (D-33). AC #41 publishes base list price only. | — |
| **Draft/sandbox bill simulation** extension (simulate a draft plan via downstream evaluation in sandbox, keeping the no-charge-computation boundary)? (F-36) | **Open** — Tariffs simulation is published-state only; consumers MUST NOT read draft (AC #50). Owner: Tariffs + Product. | — |
| **Localization** owner for Plan-owned display fields (extend the registry localization mechanism to Plan/phase/descriptor content; Presentation renders)? (F-37) | **Open** — descriptors are single-language today. Owner: Architecture + Presentation. | — |
| **Customer-group** pricing (per-`(group, region)`) a launch/migration requirement? (F-41, reopened as F-88) | **Answered — yes, in launch Scope (MAJOR)** — adjustment-based `customerGroup` `PriceOverlay` axis (resolved via `payerTenantId`; BSS-owned effective-dated audited membership; §17.7 + AC #109). Different per-group tier *structures* stay Future. Owner: Product. | 2026-07-04 |
| Self-service **term / auto-renew** metadata home (optional `termLength`/`autoRenew` on the Plan)? (F-35) | **Answered — no** (not at launch); explicit boundary added (Out of Scope) — term/auto-renew is not a catalog concern at MVP. Owner: Product + Subscriptions + Contracts + Billing. | 2026-07-04 |
| Unify purchasability dating into one field pair, and decide whether **deferred publish** ("publish at T") is a launch need? | **Answered — unify: yes** (one `availableFrom`/`availableTo` pair for **all** billing cycles); **deferred publish: no** (out of launch scope). Owner: Product + Architecture. | 2026-07-04 |
| Document owner + target release? | **Open** — assign a document owner and target-release milestone before Design. Owner: Program/Architecture. | — |

## 16. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Tax Engine slips beyond its post-MVP ETA | Tax-inclusive rows/markets stay not-sellable-GA; `region`->jurisdiction mapping deferred | MVP sells tax-exclusive; `taxInclusive=true` authorable but GA-gated (AC #18/#19); track Tax Engine GA on the program board |
| ~~`CatalogVersion` increment taxonomy / batching SLO unconfirmed by Registry~~ — **resolved (D-47, 2026-07-28)** | `pricingSnapshotRef` determinism and the AC #95 propagation SLO undefined | Closed: taxonomy + SLO ratified (D-47: interactive <= 5s coalescing; bulk <= 5 min hard max; p95 pending→committed <= 60s), mirrored in the products-gear PRD |
| Proration / plan-change enum drift across Subscriptions and Tariffs | Wrong credits on up/down-grade; divergent proration | Canonical `prorationBasis` enum owned here and adopted verbatim; shared golden fixtures (AC #61) before code |
| Conformance fixtures (tier-boundary, package, per_unit, proration, reserved, supersession-continuity, level-aggregation) not stood up before implementation | Off-by-one / model-kind mispricing reaches production | Publish blocked for any `modelKind` — or non-`sum` `aggregationFunction` row (D-44) — lacking a joint golden fixture (AC #60); fixtures version-controlled before code |
| Promotions PRD does not yet exist | No coupon authoring/evaluation owner at launch | `discountRef` committed as the day-1 hook (referential-integrity only, AC #69); full Promotions PRD remains the durable owner |
| Prepaid balance execution (ledger/drawdown/auto-recharge) absent | Prepaid grants definable but not sellable/consumable | Catalog defines the grant only (frozen in snapshot); GA-gate the sellable path on Billing/Rating balance execution |
| Read-model availability/DR targets unproven under load | Rating hot-path resilience unproven | Fail-closed on read-model outage (AC #103); targets ratified 2026-07-28 (99.9%/5m/30m), verified by the DR drill + load test |
| ~~Upstream SKU retirement joint contract open~~ — **closed by cross-reference (D-47, 2026-07-28)** | Draft/published plans may reference a retired SKU | Closed: the registry never retires a SKU with active plan references (products PRD `fr-sku-lifecycle` `SkuReferenceCount` predicate, fail-closed); pricing flags referencing plans and blocks new adoption (AC #82) |

## 17. Reference Materials

| **Material** | **Link / path** | **Comments** |
|--------------|-----------------|--------------|
| Product & SKU Management PRD (**products** gear) | `gears/bss/products/docs/PRD.md` (vendored 2026-07-16 from upstream PR #4177) | Catalog registry (SoR for Product/SKU/Category/Attribute/`CatalogVersion`, `bundle` SKU type, metering-unit declaration, `PlanTier` taxonomy) this PRD builds on |
| Rating PRD (**rating** gear — incl. the evaluation core, former "Tariffs / PLAL") | `gears/bss/rating/docs/PRD.md` | Formulas, hierarchy, `pricingSnapshotRef`, promotions/FX boundary; Usage -> billable items |
| Subscriptions & Entitlements PRD (**subscriptions** gear) | `gears/bss/subscriptions/docs/PRD.md` | `PlanLink`, migration execution, plan-change runtime |
| Billing Ledger & Balances PRD (**ledger** gear) | `gears/bss/ledger/docs/PRD.md` | Posted invoice immutability |
| Usage Collector PRD (**usage-collector** gear) | `gears/system/usage-collector/docs/PRD.md` | Usage collection; the phase-2 feed the rating seams consume is a **rating-PRD commitment**, not yet authored in the collector's design set |
| BSS Architecture Manifest | `docs/bss/manifest/vz-arch-manifest-bss-only.md` | **Historical — not vendored into this repo**; §4.1 Catalog; §4.2 Rating; §4.3 Subscriptions |
| Product Catalog & Marketplace PRD | `docs/bss/prd/PRD-product-catalog-marketplace-202601120119/…` | **Historical — not vendored**; parent catalog, SKU lifecycle (superseded by the products gear row above) |
| Contracts & Agreements PRD | `docs/bss/prd/PRD-contracts-agreements-202601120119/…` | **Historical — not vendored**; contract locks (no gear exists yet) |
| Plan & Price Modeling use case | `docs/bss/prd/PRD-product-catalog-marketplace-202601120119/UC-plan-price-modeling-202601121200.md` | **Historical — not vendored**; scenario reference, superseded |
| Effective Dating & Price Windows use case | `docs/bss/prd/PRD-product-catalog-marketplace-202601120119/UC-effective-dating-price-windows-202601121200.md` | **Historical — not vendored**; superseded/absorbed (consolidated here), scenario source material |
| Project glossary | `docs/project-glossary.md` | **Historical — not vendored**; canonical terms |
| Trace chain | `AGENTS.md` (repository root) | Manifest -> PRD -> ADR -> Design -> Stories |

### 17.1 Supported Billing Cycles and Price Structure Kinds (catalog)

**Supported billing cycles**

| **Billing cycle** | **Intent** | **Catalog requirements** |
|-------------------|------------|---------------------------|
| **One-time** | Single purchase | Exactly one `one_time` base price row **per sold `(currency, region)`** — the *at most one* half is the canonical scope key's uniqueness (§2.2), the *at least one* half fails publish `BASE_MARKET_INCOMPLETE` (**D-149**, 2026-08-03: the rule was stated and unenforced, so a market could be sellable with nothing to charge for); optional quantity min/max; optional availability window; MUST NOT attach a recurring-only add-on — an add-on SKU none of whose published plans carries a `one_time` base row, refused `ADDON_INCOMPATIBLE` by the composition step, which is where the add-on's own plans are read (D-149) |
| **Recurring** | Subscription fee (`monthly`, `quarterly`, `semiannual`, `annual`, or `customEveryN{Days\|Months}(n)`) | ≥ 1 recurring base price row **per sold `(currency, region)`** (`BASE_MARKET_INCOMPLETE` otherwise — **D-149**); links to `PriceWindow`; supports plan phases and `per_unit` (per-seat) pricing; `billingTiming` (`in_advance`\|`in_arrears`) REQUIRED at publish; MAY declare an optional one-time setup row; `frequency` and any custom interval `n` persisted as metadata — an absent `frequency` fails publish `CYCLE_METADATA_MISSING` (D-149), as does a plan declaring **no billing cycle at all**, on any cycle, since every rule of the matrix is conditioned on the cycle and an undeclared one passed all of them vacuously (publish MUST also reject non-positive `n`) |
| **Usage-based** | Charge by metered consumption | Requires parent SKU `meteringUnit`; tier bands or an untiered `per_unit` rate; MUST set `billingGranularity` on **all** usage rows (untiered or tiered); MUST set `tierAggregationWindow` **when tiered or `package`** (D-58) |
| **Hybrid** | Base fee + variable usage | Same `planId` MUST declare both recurring and usage price components; publish validation MUST reject hybrid without both parts; MAY additionally declare an optional one-time setup/activation row |

**Supported price structure kinds (catalog flags)**

| **Kind** | **When used** | **Catalog MUST persist** |
|----------|---------------|---------------------------|
| **Flat** | Fixed unit or period price | `modelKind=flat`; single amount or unit price; **non-usage rows only** (`flat` on a usage row fails publish — D-18 matrix) |
| **Per-unit (per-seat)** | Unit price x quantity — on a **non-usage** row the quantity is external (seats/users); on a **usage** row it is the metered `Q` (the plain untiered metered rate) | `modelKind=per_unit`; unit price + `quantitySource` (`subscription_seat_count` \| `manual`) on **non-usage** rows, where the quantity is resolved by Subscriptions/operator and **never metered here**; on a **usage** row `quantitySource` is absent and `billingGranularity` is REQUIRED |
| **Graduated** | Marginal tier pricing | `modelKind=graduated`; ordered non-overlapping bands |
| **Volume** | Single rate on total quantity | `modelKind=volume`; ordered non-overlapping bands |
| **Package (block)** | Usage billed in whole blocks (e.g. 100 units for $80; 150 used -> 2 blocks -> $160) | `modelKind=package`; `packageSize` (> 0) + `packagePrice` (>= 0); tier-band fields absent; round-up math in Tariffs |
| **Tiered (unspecified)** | — | MUST NOT publish; operator MUST choose graduated or volume |

Mathematical formulas belong in Tariffs Design; catalog MUST NOT compute charges.

### 17.2 Model Kind / Tariffs Formula Mapping (conformance)

The catalog `modelKind` enum and the Tariffs formula matrix MUST reconcile one-to-one; publish of any `modelKind` lacking a **joint golden conformance fixture** MUST be blocked (AC #60). This table is the **single** kind-to-formula source of truth — Tariffs and this PRD MUST NOT diverge from it:

| **Catalog `modelKind`** | **Tariffs formula** | **Status / joint fixture** |
|-------------------------|---------------------|-----------------------------|
| `flat` | Flat | Exists |
| `graduated` | Tiered (graduated) | Exists (AC #60 boundary fixture) |
| `volume` | Volume — **Variant A** (single rate on total `Q` within `tierAggregationWindow`) | Exists; catalog `volume` maps to **Variant A only** |
| `package` | **Repeating-block** (`blocks = ceil(used / packageSize)`, charge `= blocks x packagePrice`) | **Present in Tariffs**; distinct from Volume Variant B (stair-step per-tier flat fee); joint golden fixture + Tariffs AC pending (AC #60/#88) |
| `per_unit` (per-seat) | **External-quantity** (`unitPrice x quantity` from `quantitySource`) | **Present in Tariffs**; joint golden fixture + Tariffs AC pending (AC #60) |
| `usage` row + `reservedRate`/`reservationFlavor` (reservation variant — **not** a distinct `modelKind`) | **Reserved-rate + on-demand** (reserved rate on matched/allocated quantity, on-demand rate on the remainder; `capacity` flavor bills the allocation regardless of usage) | **Evaluation exists** in Tariffs (step 6); the change needed is **sourcing** — Tariffs MUST take the **self-service** reserved rate from the catalog row (snapshot-frozen) — Tariffs change + joint fixture (AC #60) |
| `usage` row + `aggregationFunction ∈ {peak, time_weighted}` (level variant — **not** a distinct `modelKind`; D-44) | **Granule fold** (`Q` = Σ per-`aggregationGranularity` folds: `peak` = max gauge sample per granule; `time_weighted` = step-integral with bounded `hold_last`), then the row's band/flat math over the additive `Q` (rating T-D-17) | **New in launch (2026-07-16)** — rating `fr-level-aggregation` + AC 5d; joint golden fixture required (AC #60: granule fold, late-sample re-fold, `maxHold` gap); publish of a non-`sum` row blocked without it |

Tariffs' **Volume Variant B** (per-tier block fee) is **dropped** (decided 2026-07-04) — it was not authorable here (catalog tier bands carry unit prices only, no per-band flat-fee field); catalog `volume` maps to **Variant A** only.

### 17.3 Plan Composition Rules (normative)

| **Rule** | **Statement** |
|----------|---------------|
| Parent SKU | Plan MUST reference a **published** SKU; draft SKU MUST block plan publish |
| PlanTier | Every Plan MUST declare `PlanTier` before publish (optional at draft); the taxonomy and SKU-level value are owned by the registry. Publish MUST validate that the Plan's `PlanTier` **equals its parent SKU's `PlanTier`** unless an **explicit, audited override** is declared (default: equal) — no silent divergence (AC #7) |
| Meter injective | Exactly **one priced line per `(meter, dimensionKey)` per scope-key slice**; a duplicate line within one slice MUST fail publish. A plan MAY price **several** `meteringUnit`s (D-103). **Multi-unit single-line** offerings are modeled as a **derived (composite) meter** (one output unit); separate single-meter SKUs composed via bundle/add-ons remain available (both in launch Scope) |
| Add-on compatibility | Add-on SKUs MUST be published and compatible with base SKU; conflicts MUST fail publish. A **recurring-only** add-on on a one-time base plan is one of those incompatibilities (`ADDON_INCOMPATIBLE` — **D-149**, 2026-08-03: §17.1's one-time row had stated the ban with no code and no owner, and deciding it needs the add-on SKU's own plans, which only this step reads) |
| Add-on quantity bounds | An add-on rule's bounds MUST admit a selection: a **required** add-on MUST have `maxQty >= 1`, `minQty <= maxQty` whenever both are set, and `stepQty > 0` whenever set — each failing publish as `ADDON_QTY_RANGE_INVALID` naming the bound (**D-150**, 2026-08-03: only the first was stated, and only as a schema check with no code, so an author met it as an internal fault and the other two admitted an add-on nobody can select on a plan that publishes clean) |
| Bundle components | All `includedSkuIds` MUST be published; rev-share MUST sum to 100% per vendor SKU when set |
| Bundle price basis | A Bundle MUST declare its price basis: `sum_of_parts` or `own_price`; the basis and any explicit price MUST be persisted and frozen. For `sum_of_parts` the bundle MUST reference the specific **component `planId`s** (not bare `skuId`s) whose rows are summed, and publish MUST validate that **every** referenced component has a **covering published price row in each `(currency, region)` the bundle sells in** (and matching `frequency` for recurring components). A missing or ambiguous component row MUST fail publish. Itemization is independent of the basis; **rev-share requires `sum_of_parts`** (D-55 — an `own_price` bundle has no per-vendor-SKU allocation base; `own_price` + rev-share fails publish until an allocation base is decided, a named Future gate) |
| Billing descriptors | Publish MUST include the complete billing descriptor set per manifest §4.1 / D-48 v1 (**three** descriptor-set fields; `billingTiming` **and** `taxCategory` ride the price row — D-110) |
| Price window coverage | For billable usage at time `t`, an active `PriceWindow` MUST exist for the resolved **canonical scope key** (resolved on the **base** `priceOverlay`) or Tariffs step 2 MUST fail (no silent fallback). Because `priceEligibility` and `chargeKind` are part of the key, a grandfathered row and its successor, and the `recurring`/`usage`/`one_time_setup` components of a hybrid plan, are **distinct keys** that MAY each hold an active window at the same `t` |
| Hybrid completeness | Hybrid plans MUST include at least one recurring and one usage price row |
| One-time setup charge | Recurring and hybrid plans MAY declare an **optional one-time setup/activation price row** on the same `planId`, charged **once per subscription lifetime** (at activation; at trial conversion for trialed plans; never re-charged on plan change/migration) and frozen in `pricingSnapshotRef`. It is a **first-class plan price row** — **not** a synthetic add-on SKU. Publish MUST validate it as one-time (no recurrence, no `billingTiming`/tier fields) |
| Sellability gate | A subscription/purchase MUST NOT be created unless all **six** predicates of `fr-sellability-gate` hold, at `t`, for **every** canonical scope key the purchase binds on the bound `(currency, region)` — the `recurring`/`usage`/`one_time_setup` components and every phase of the chain, eligibility-resolved, grandfathered generations excluded; one failing component key blocks the plan-market, never a partial sale (D-94 — this row had kept the pre-D-94 singular "the bound canonical scope key", 2026-07-31 review fix): (1) active (not merely *scheduled*) `PriceWindow`; (2) addressable in a **committed `CatalogVersion`**; (3) `availableFrom`/`availableTo` open; (4) lifecycle not retired; (5) no GA-gate flag on the bound market; (6) registry `sellable = true` for a **standalone** line (D-46; bundle components exempt) — a joint rule with Subscriptions. Plans of **any billing cycle** MAY declare optional plan-level `availableFrom`/`availableTo` (validated against window coverage at publish); **deferred publish** is out of launch scope |

### 17.4 Price Validation Rules (catalog)

| **Rule** | **Validation** |
|----------|----------------|
| Amount sign | Price amount MUST be **>= 0**. A `0` amount is valid for free tiers, `trial`/`interim` phases, and the first graduated band. Negative amounts MUST be rejected (typed credit/discount rows are Future scope). |
| Currency | MUST be valid ISO 4217 |
| Currency coverage (no implicit FX) | Catalog MUST NOT perform FX conversion. If no price row exists for a requested `(currency, region)`, preview/publish MUST fail closed; no base-currency fallback unless an explicit `currencyFallbackPolicy` is configured (Future). FX is owned by Tariffs/PLAL. |
| Precision | Amount precision MUST follow the **ISO 4217 minor-unit** for the row's currency (0 for JPY/KRW, 2 default, 3 for BHD/KWD/OMR); publish MUST reject amounts with more precision than the currency's minor unit. A flat 2-decimal cap MUST NOT be assumed. |
| Rounding policy | Every published price row MUST resolve a rounding policy — the row-level reference or the tenant default; if neither exists, publish MUST fail (no implicit rounding). **The tenant arm became reachable on 2026-08-15 (D-320)**: `default_rounding_policy_ref` had no writer until `GET/PUT /bss-pricing/v1/config/rounding-policy`, so every catalog had to restate the reference on every row |
| Duplicate scope | MUST NOT allow a duplicate price row on the **canonical scope key** without the supersession workflow. Rows differing only by `priceOverlay`, `phase`, `priceEligibility`, `chargeKind`, or `cohort` (e.g. the `recurring` and `usage` components of a hybrid plan, or two grandfathered generations) are **distinct**, not duplicates. `cohort ≠ none` on a non-grandfathered row MUST fail publish |
| Tier order | Tier bands MUST have ascending `fromQty`; ranges MUST NOT overlap |
| Tier contiguity | Tier bands MUST be contiguous (no coverage gaps) under the `[fromQty, toQty)` convention; the top band MUST be **open-ended** (`toQty=null`) — a closed top band MUST fail publish (D-17): quantity capping is owned by entitlement quotas (Subscriptions enforces), per-period fee caps are Tariffs Future; any quantity is always rateable |
| Per-unit quantity source | `modelKind=per_unit` **non-usage** rows MUST persist a `quantitySource` (`subscription_seat_count` \| `manual`); catalog MUST NOT infer or compute the quantity. On a `per_unit` **usage** row the meter supplies `Q`, so `quantitySource` MUST be **absent** — presence MUST fail publish (D-57; S3 `inst-mk-required`) |
| Evaluation-policy placement | `tierAggregationWindow` and `billingGranularity` are **usage-row only**; on `flat` and on `per_unit` **non-usage** rows they MUST be **absent** — presence MUST fail publish. A `per_unit` **usage** row (the plain untiered metered rate) carries `billingGranularity` like every usage row. `tierAggregationWindow` is REQUIRED on **tiered** usage rows **and on `package` usage rows** (D-58 — block round-up is non-linear in the window); absence MUST fail publish (`EVAL_POLICY_MISSING`) |
| Region taxonomy | A price row's `region` MUST be a member of the tenant's configured region taxonomy; an unknown/invalid region MUST fail validation before publish |
| Brand taxonomy | `brand` is a `PriceOverlay` scope value, **not** a price-row field. A **brand-scoped `PriceOverlay`**'s `brand` MUST be a member of the tenant's configured brand taxonomy; an unknown/invalid brand MUST fail validation before publish (AC #75) |
| Tax basis completeness | `taxInclusive=true` without region tax readiness **and** `taxInclusive=false` in a region with no configured `taxCategory` are both governed by the tenant tax-display policy (warn/fail; default fail-closed); readiness = the tenant-declared per-`(tenant, region)` config (Tax Engine-verified post-GA) |
| Min-quantity floor | A price row's `minQtyThreshold` MUST declare its **floor type** — `purchase` (Subscriptions rejects orders below the floor) or `usage` (Tariffs/Rating treats usage below the floor as ineligible and fails closed, **not** silently zero-rated; the `usage` floor declares its fallback on the row — launch: `exception`, the rating exception path). Publish MUST reject a `minQtyThreshold` with no declared floor type or a `usage` floor with no declared fallback, and MUST warn if it falls inside a non-zero-priced band |
| Package pricing | `modelKind=package` MUST persist `packageSize` > 0 and `packagePrice` >= 0; tier-band fields MUST be absent; publish MUST reject otherwise |
| Prepaid grant | If a plan declares a prepaid credit grant, `expiryPolicy` MUST be set (`never` or `days(N>0)`) and `creditUnit` MUST reference a currency or a **published** `meteringUnit`; a `promotional` grant MUST carry no price rows and no `autoRechargeAllowed`; `applicability` MUST resolve to published usage lines of the plan (materialized at publish; a metered `creditUnit` bounds it to that unit — D-43); catalog MUST NOT persist any balance |
| Tax-inclusive | If `taxInclusive=true`, region tax readiness (tenant-declared rate-present marker; Tax Engine-verified post-GA) MUST exist or publish MUST warn/fail per the **tenant tax-display policy** |
| Required add-on | Required add-on MUST have `maxQty >= 1` |
| Instant precision | Every instant the catalog authors, carries in a contract field, publishes, or compares MUST be **UTC at millisecond resolution** — cutover instants and the `cohort` axis derived from them, `PriceWindow` `effectiveFrom`/`effectiveTo`, `grandfatherUntil`, `availableFrom`/`availableTo`, and the D-88 changeover instant (§4.1). An authored instant with finer precision MUST fail validation (`TIMESTAMP_PRECISION_EXCEEDED`, 422) and MUST NOT be truncated — the same posture the **Precision** row above takes on money, for the same reason: truncation moves an instant a scope-key axis, a window bound and an approval-time floor are all derived from, and a truncating producer agrees with a non-truncating consumer until the day it does not (**D-144**, 2026-08-02 implementation finding · joint with Rating — D-126's cohort bootstrap compares at this quantum). Storage bookkeeping no operator authors (`created_at`, audit-chain and outbox timestamps) is outside the rule |
| Grandfathering horizon | `grandfatherUntil` MUST be non-null **only** on a row whose `priceEligibility = existing_grandfathered`; a value on any other row MUST fail publish (`GRANDFATHER_UNTIL_FORBIDDEN`, 422 — **D-147**, 2026-08-02 implementation finding). It is the sibling of the `cohort ≠ none` ⇔ `existing_grandfathered` rule in **Duplicate scope** above: one axis-conditioned field, one code. Before it the pairing was a physical column check standing behind no stated rule, so its violation surfaced as an internal fault on a request the operator could have reshaped. A grandfathered row with a null bound is **indefinite**, and the field is **not** a general per-row availability bound — `PriceWindow` `effectiveTo` and the plan's `availableTo` own that |

### 17.5 Price-Change Mechanisms and CatalogVersion Increment Contract

**Price-change mechanisms (versioning, windows, supersession)** — four mechanisms describe price change over time; they are **distinct and composable**, not alternatives:

| **Mechanism** | **What it does** | **Artifact** | **Event** |
|---------------|------------------|--------------|-----------|
| Plan **versioning** | Captures a structural/price change as a new immutable revision; prior rows retained as history | New `Price` row(s) + new plan revision | `PlanUpdated` / `PriceCreated` |
| **Supersession** | Replaces the active row for one **canonical scope key** with a newer one | New `Price` row marked superseding; prior row closed | `PriceUpdated` |
| **`PriceWindow`** | Schedules **when** a versioned/superseded row is effective | `PriceWindow` linkage + schedule (store, state machine, and activation owned by the pricing gear — Slice 7, D-03) | `PriceWindowScheduled` / `Activated` / `Cancelled` / `Expired` |
| **Grandfathering cutover** | Atomically **closes the current `all_subscriptions` window by shortening its `effectiveTo` to the cutover** (active windows are **not** cancelled) and creates (a) an `existing_grandfathered` **copy** — a new `cohort` **generation** (repeatable: each cutover creates one; prior generations untouched) — for pre-cutover subscriptions and (b) the `all_subscriptions` successor, as **one approval unit** so no coverage gap opens. A cutover MAY span **multiple scope keys** of the plan at one instant as one approval unit (per-key generations) | Shorten current window `effectiveTo` + schedule `existing_grandfathered` copy + schedule successor (one atomic unit) | `PriceCreated` x2 (grandfathered copy + successor) + `PriceWindowScheduled` x2 + `PriceWindowExpired` at cutover; `PriceWindowCancelled` only for *not-yet-active* windows of the old key |

Normative relationship: **supersession is versioning scoped to one canonical scope key** — it MUST create a new immutable `Price` row (never mutate in place) and MUST open/close the corresponding `PriceWindow` rather than overlap it, executing only as the atomic **supersession unit** (D-88: successor row + window shorten + window schedule, one approval unit, one transaction, changeover instant ≥ approval commit + the max batching-delay SLO). Supersession operates **within one `priceEligibility` class and one `chargeKind`**, and on usage rows it additionally preserves `modelKind`, `packageSize`, the unit/counter-determining evaluation-policy fields, and — on a `carry`-allowance row — `includedAllowance` (D-82 + D-98 + D-122 + D-129) — supersession changes the price, never what or how the key meters, which formula prices it, or what plan-scoped artifact it compiles to. **That guard binds the key, not the mechanism (D-127, 2026-08-01 review fix):** the grandfathering cutover's `all_subscriptions` successor lands on the predecessor's own canonical scope key and inherits the same continued tier counter, so it carries `supersedesPriceId` and passes the identical check — without it a cutover could flip `per_hour → per_day` or `graduated → volume` on a live counter, the band-edge class already closed for supersession, kind flips, the phase axis and plan change. An `existing_grandfathered` row is **immutable in price** — it MUST NOT be superseded; the **only** permitted mutation is **setting or tightening `grandfatherUntil`** (never loosening, never the price), which is a **material change** (AC #28). No mechanism may produce overlapping active `PriceWindow` rows for **one** canonical scope key (manifest invariant, as extended in §2.2).

**CatalogVersion increment contract (with registry)** — `CatalogVersion` is owned by the registry; this PRD **freezes** plan/price/descriptor content into it but does not re-define it:

| **Change class** | **New `CatalogVersion`?** | **Incrementer** |
|------------------|---------------------------|-----------------|
| Price-only edit (amount/window on existing plan) | Yes — content MUST become addressable in a `CatalogVersion` (MAY be **batched**). For a **window** edit this means the window surface is itself a **publish unit** (**D-99**, 2026-07-31 review fix: schedule / future-`effectiveTo` adjustment / cancellation run validation → pending ref → warm and re-project the affected rows' plan subject — before that rule the surface requested nothing, so the pinned read model kept advertising coverage the truth side had removed, and the sellability predicates it is resolved from went stale). Window **activation/expiry** increment nothing: the read model carries window **intervals** and "active at `t`" is derived at read time | Registry, on catalog publish request |
| Structural edit (model kind, tiers, descriptors, composition) | Yes — addressable in a `CatalogVersion` (MAY be batched) | Registry, on catalog publish request |
| **Plan retirement** (`published → retired`) | Yes — a **publish unit** in its own right (**D-128**, 2026-08-01 review fix): pending ref → plan-subject re-projection → warm, with the plan's lifecycle state a projected field. The class was **missing** from this table while the sellability gate's predicate (4) reads that state from the *pinned* read model, and a retired plan can never publish again to correct the lag — so the pin advertised it as sellable permanently. Under D-51 a plan with in-flight subscribers on every key cancels no window either, so the price-only (window) class above does not cover it | Registry, on catalog publish request |
| `PriceOverlay` / customer-group membership change | Yes — each committed mutation is a **publish unit through the engine** (validation → pending ref → warm); consumer visibility is version-pinned exactly like plan content, and the registry's batching coalesces chatty membership traffic | Registry, on catalog publish request |
| Draft-only edits (no publish) | No | — |

On **every** `PlanPublished`, this PRD MUST request that the plan's content become addressable in a `CatalogVersion`; the registry is the **sole** incrementer and **MAY batch** multiple approved publishes into **one** discretionary catalog publish. `PlanPublished` carries a **pending** version reference; the committed `CatalogVersion` is emitted as `CatalogVersionPublished`, and `pricingSnapshotRef` MUST pin that committed version (AC #27, #63). The increment-trigger taxonomy and the **max batching-delay SLO** from `PlanPublished` to `CatalogVersionPublished` are ratified (D-47: interactive <= 5s coalescing; bulk <= 5 min hard max; p95 pending→committed <= 60s) — D-47, mirrored in the products-gear PRD.

### 17.6 Consumer Contracts Detail

**Proration input contract (catalog -> Subscriptions / rating)** — this PRD owns **publishing the inputs**; Subscriptions owns the change boundary/mode and mid-cycle runtime, the rating gear the proration math, Billing the credit/adjustment artifacts. Each recurring price row MUST expose the following in the read model, frozen in `pricingSnapshotRef`:

| **Field** | **Requirement** |
|-----------|-----------------|
| `billingAnchorPolicy` | When the billing period anchors (`calendar_month`, `subscription_start`, `fixed_day(d)`); MUST be set on recurring rows; month-end/UTC handling per Glossary |
| `prorationBasis` | `calendar_days_actual` \| `calendar_days_30` \| `by_second` \| `whole_unit` \| `none` (canonical enum per Glossary, adopted verbatim by Tariffs); how partial periods are apportioned |
| `creditOnDowngrade` | Whether a mid-cycle downgrade is eligible for catalog-sanctioned credit (Subscriptions applies it). The governing value on a downgrade is the **source** row's flag, read from the subscription's **frozen snapshot** (never the target row, never the live catalog). `creditOnDowngrade = true` combined with `prorationBasis = none` is contradictory and MUST fail publish |

> **Cross-boundary mid-cycle changes (launch scope)**: Mid-cycle plan changes that **cross currency, region, or billing frequency** are **NOT supported at launch** — the proration input contract publishes **no** cross-currency/cross-frequency credit basis. Such a change MUST be handled as **cancel + new subscription**. Same-currency, same-frequency up/down-grades within one `(currency, region)` are in scope. This launch limitation is **signed off (D-49, 2026-07-28)**; GTM/customer-facing docs MUST carry the constraint entry ("changing country/currency = a new subscription"); operator warning is AC #66 — and its **wording lives there**, not in the published contract: the read model carries the machine-readable marker `crossBoundaryChangePolicy = cancel_plus_new` and no warning text (**D-169**).

**Entitlement grant set (catalog -> Subscriptions)** — the Plan MUST publish its entitlement grant set (feature flags, quotas) or its `PlanTier`-resolved reference into the read model; publish MUST fail if a referenced feature, quota, or `PlanTier` policy is undefined. This PRD does **not** define entitlement semantics:

| **Catalog concept** | **Published as (grant set)** | **Consumed by Subscriptions as** |
|---------------------|------------------------------|----------------------------------|
| `Feature` (capability id, PlanTier-driven) | `featureFlag: bool` entry | `Entitlement` (feature access) |
| Quota (limit id + value, PlanTier-driven) | `quotaKey: value` entry | `Entitlement` (usage/resource quota) |

**Plan-change contract (catalog -> Subscriptions)** — Self-service plan changes need two catalog facts this PRD MUST publish; enforcement stays in Subscriptions:

| **Field** | **Requirement** |
|-----------|-----------------|
| `allowedChangeTargets` | The target `planId`s a subscription MAY move to — an **explicit list** of published `planId`s (rule-based targets are not authorable at launch — Future scope, D-23; §17.8). Absence MUST mean **no self-service change** (fail-safe), **not** any-to-any |
| `comparabilityRank` | An integer rank classifying a change as **upgrade** (higher), **downgrade** (lower), or **switch** (equal). REQUIRED for **every** plan that participates in self-service change — `PlanTier` is **never** an ordering at launch (S6 K4; an authoritative published `PlanTier` ordering is §17.8 Future) |

**Tenant policy objects (catalog governance)** — both have fail-safe defaults:

| **Policy object** | **Purpose** | **Safe default** |
|-------------------|-------------|------------------|
| Approval threshold policy | Sets the **material-change** threshold above which the two-person rule applies. Materiality MUST be an absolute amount or a percentage delta (per currency). For a multi-currency change, each affected row's delta is compared in its **own** currency and the rule trips if **any** row exceeds its threshold (AC #64). Mutating the policy is itself **always material** (independent second approver required — D-10). | If unset, the **two-person rule applies** (fail-safe); the system MUST NOT auto-publish without an explicit threshold |
| Tax-display policy | Decides warn vs. fail when `taxInclusive=true` lacks a region tax rate **and** the symmetric `taxInclusive=false` row in a region with no configured `taxCategory`; governs only the **display basis** | **Fail closed for ALL tenants**; tax **scheme** determination is owned by Tax Engine |

**Rating compatibility contract (normative)**

| **Requirement** | **Statement** |
|-----------------|---------------|
| Stable identifiers | Published plans MUST expose `planId`, `priceId`, `skuId` on all downstream artifacts |
| Snapshot completeness | Publish MUST stamp identifiers sufficient for manifest `pricingSnapshotRef` |
| Model kind | Rows MUST persist `flat` \| `per_unit` \| `graduated` \| `volume` \| `package` — no implicit default at rating time; `per_unit` rows MUST also persist `quantitySource`; `package` rows MUST persist `packageSize`/`packagePrice`; `graduated`/`volume`/`package` are valid on **usage rows only** (D-18) |
| Evaluation policy | Usage rows MUST persist `billingGranularity` at publish; tiered usage rows MUST additionally persist `tierAggregationWindow` |
| Billing timing | Recurring rows MUST persist `billingTiming` at publish; usage rows are implicitly `in_arrears`; absence on a recurring row MUST fail publish |
| Meter mapping | Exactly one `meteringUnit` per usage plan revision; ambiguous config MUST fail publish. A **derived (composite) meter** satisfies this as **one output unit** |
| Derived meter (when used) | If a price row uses a derived meter, publish MUST persist constituent units, formula-as-data, and output unit, and MUST fail if any constituent `meteringUnit` is unpublished or the formula self-references; Tariffs evaluates the formula |
| Events | MUST emit `PlanCreated`, `PlanUpdated`, `PlanPublished`, `PlanRetired`; ordered per `(tenantId, aggregateId)` where manifest applies |
| No charge calculation | Catalog MUST NOT compute monetary charges; Tariffs/Rating consume frozen inputs |
| Descriptor completeness | Publish MUST NOT proceed without billing descriptor set |

### 17.7 Advanced Pricing Primitives Detail

**Derived (composite) meter (catalog primitive)** — one price row priced from a **formula across multiple published metering units** (canonical case: VM priced as `vCPU` + `RAM` on one line). This PRD owns the **definition primitive**; Tariffs owns the **evaluation**:

| **Catalog MUST persist** | **Requirement** |
|--------------------------|-----------------|
| Constituent units | >= 2 **published** `meteringUnit` ids (declared by the registry); each MUST be published before the composite can publish |
| Formula expression | The combination rule **as data** (operands + operator/weights), not executable code; catalog stores it, Tariffs evaluates it |
| Output unit | A single declared **derived unit** the price row rates as one line |
| Determinism | The composite definition MUST be **frozen in `pricingSnapshotRef`** so rating is reproducible |
| No computation here | Catalog MUST NOT compute the formula result; it only persists and publishes the definition for Tariffs |

**Prepaid credit grant (catalog primitive)** — this PRD owns the **definition**; balance ledger, drawdown, zero cut-off, and auto-recharge are owned by Billing/Rating (External-dependency GA gate):

| **Catalog MUST persist** | **Requirement** |
|--------------------------|-----------------|
| `grantAmount` | Units granted (> 0), expressed in `creditUnit` |
| `creditUnit` | A currency (ISO 4217) **or** a **published** `meteringUnit`; an unpublished unit MUST fail publish |
| `price` per `(currency, region)` | (`category = prepaid` only) Purchase price of the grant (>= 0), authored **per `(currency, region)`**. The grant is a **plan-attached primitive**, **not** a `Price` row on the canonical scope key — it carries **no** `chargeKind`. A single unscoped price MUST fail publish for a plan selling in multiple `(currency, region)`. Grant-price changes flow through the **same** material-change policy |
| `expiryPolicy` | `never` or `days(N)` with N > 0 on **authored** grants; MUST be set explicitly (no implicit "never"). **`periods(N)`** (anchor-derived billing periods) is a **compiled-only** third form — REQUIRED on `source = compiled_allowance`, forbidden on authored grants (**D-52**) |
| `source` | `authored` \| `compiled_allowance` (**D-52**). Grants are a **table, not a singleton**: one plan legitimately holds several — an authored prepaid grant plus one compiled `carry` allowance grant **per** allowance-carrying usage row (D-45 `inst-ac-carry`). A compiled grant records its source `price_id` for replay lineage and is recompiled at publish, never hand-edited |
| `autoRechargeAllowed` | Whether auto-recharge MAY be offered (execution owned by Billing); `category = prepaid` only — a recharge is a purchase |
| `category` | `prepaid` (default — purchased at the grant price) **or** `promotional` (issued **free** — price rows MUST be absent; `expiryPolicy = never` warns). Frozen in the snapshot (D-43) |
| `applicability` | The usage lines the credit may offset at drawdown: `all_usage` (default) or a set of **published** `meteringUnit` ids that are usage lines of the plan (never `one_time_setup` or recurring rows — launch rule; a metered `creditUnit` bounds the set to that unit's meters). Publish **materializes** the resolved set into the snapshot — the executor never infers scope (D-43) |
| `drawdownPriority` | Optional int ≥ 0 (lower draws first) — an authored **default rank**. The **effective** order across an account's grants is **Billing-owned**: `drawdownPriority` → `promotional` before `prepaid` → earlier expiry → earlier issuance → `grantId` (deterministic total order, D-43) |
| Determinism | The grant definition MUST be **frozen in `pricingSnapshotRef`**; catalog MUST NOT track balance, compute drawdown, or order live balances |

**Reserved-capacity price (catalog primitive)** — reservation is modeled as **attributes on the single `usage` price row**, **not** a second row, so it does not price the same `(meter, dimensionKey)` twice:

| **Catalog MUST persist (on the usage row)** | **Requirement** |
|---------------------------------------------|-----------------|
| `reservedRate` | Committed unit price (>= 0, row currency) for the reserved/allocated quantity, carried **alongside** the row's on-demand unit price/tiers |
| `reservationFlavor` | `consumption` (matched usage at `reservedRate`, remainder on-demand) **or** `capacity` (allocated quantity billed at `reservedRate` regardless of usage — `capacityCharge`, which accrues **per covered granule**: `Σ reservedRate × reservedQuantity_i × duration_i` over the reservation's covered sub-intervals in the period, since `reservedRate` is money per billable unit and not a period charge — **D-139**, adopting rating T-D-25; `coveredGranules` is Rating-computed runtime coverage, never authored or frozen here); aligned field-for-field with Tariffs `reservationMatch`. On a **non-`sum` row** only `capacity` is authorable — `consumption` fails publish (`LEVEL_RESERVATION_CONSUMPTION_FORBIDDEN`, D-53) |
| Evaluation policy | The row **is** a usage row: `billingGranularity` is REQUIRED, and `tierAggregationWindow` is REQUIRED **when tiered or `package`** (D-58) — the "forbidden on non-usage rows" rule does **not** apply |
| Quantity source | The reserved/allocated quantity is supplied at runtime (OSS/Contracts entitlement/inventory); catalog MUST NOT meter or compute it |
| Determinism & fixture | Frozen in `pricingSnapshotRef`; reservation is a **Tariffs evaluation variant** of the usage row and MUST carry a joint golden fixture before publish (AC #60; §17.2). Catalog MUST NOT track allocation or compute the charge |

**Boundary**: **negotiated RI-style** reservation rates are carried by Contracts (per-account overlay); **self-service** reservation rates are catalog list pricing (this primitive). Self-service reservation is **in launch Scope**. Tariffs reservation evaluation exists (step 6) and MUST source the **self-service** reserved rate from the catalog snapshot.

**Customer-group pricing (segment overlay)** — modeled as a **`customerGroup` `PriceOverlay` scope**, **not** a new price-row axis:

| **Aspect** | **Requirement** |
|------------|-----------------|
| Mechanism | A **`customerGroup`-scoped `PriceOverlay`** carries a **line set** (D-42: per `(planId?, targetSku?)` line its own `markup`/`discount`/`fixed` magnitude; per-currency values for amount kinds — D-08) per **group**, resolved via the payer's group membership on **`payerTenantId`** at `t` (Tariffs evaluates the most-specific line per priced row); region-differentiated treatment = the region variance of the underlying base rows. Entirely different **tier structures** per group are **out of launch scope** (use separate plans — Future) |
| Group taxonomy | A **BSS-owned governed taxonomy** (like `region`/`brand`); group values validated against it at authoring |
| Membership | An **effective-dated, audited BSS record** on the payer's commercial profile (resolved via `payerTenantId`); **at most one active membership per payer across all groups** — a transfer is one atomic audited **move** (end + start), so resolution is unique by construction. AMS supplies tenant identity only; the group is a **BSS commercial projection** and MUST NOT change tenant topology |
| Determinism | The **resolved group** MUST be frozen in `pricingSnapshotRef` |
| Membership change | A price-changing operation: **renewal-aligned by default** (pinned in the subscription snapshot until renewal, then re-resolves); an **immediate** re-resolution is allowed as an explicit **material change**. A group discount/move affecting many payers is a **material change** -> two-person rule (AC #28); all membership changes MUST be audited |

Enforcement (overlay evaluation, precedence stacking) is owned by Tariffs; this PRD owns the `customerGroup` `PriceOverlay` **authoring/validation**, the group **taxonomy**, and publishing the **membership** record into the read model.

### 17.8 Future Scope

| **Capability** | **Priority** | **Status** | **Notes** |
|----------------|--------------|------------|-----------|
| Rule-based `allowedChangeTargets` (read-time fail-safe resolution + `partially_resolvable` marker) | `p2` | Follow-on | Launch = explicit lists only (D-23); a rule's targets exist only at read time, so publish-time guarantees need the designed read-time semantics first |
| Authoritative published `PlanTier` ordering (as an alternative to per-plan `comparabilityRank`) | `p3` | Follow-on | Cut at launch (2026-07-28 review fix, S6 K4): the alternative named no artifact, no API and no publish check, so `comparabilityRank` is REQUIRED for **every** plan in self-service change. Reintroduce only with a tenant-wide ordering object + a publish check that every change-graph target is covered by it |
| Multi-group membership (concurrent customer-group memberships + winner/stacking rule) | `p2` | Follow-on | Launch holds **one active membership per payer** (D-09); revisit together with Promotions discount-stacking if a real need appears |
| Tiered per-seat pricing (`graduated`/`volume` bands over seat count on recurring rows) | `p2` | Follow-on | Requires extending `quantitySource` semantics to banded kinds (D-18); until then seat pricing is single-rate `per_unit` |
| Closed top tier band (author-acknowledged fail-closed maximum) | `p3` | Follow-on | Forbidden at launch (D-17 — top band always open; capping = quotas / per-period caps); reintroduce with an explicit author-acknowledged marker only if a real SKU needs "price undefined above X" |
| Plan comparison (side-by-side) | `p2` | Follow-on | Operator/partner decision support |
| Committed-usage / drawdown flags on plan | `p2` | Cross-PRD | Contracts + Tariffs; catalog may expose reference fields |
| Minimum fee / cap per period on plan | `p2` | **Decided → Scope (2026-08-15, D-319)** | Re-scoped into **launch**: the deferred part was only ever the **catalog authoring field**, and it has landed as §6.1 `fr-period-floor-cap` — a revision-scoped plan-level floor/cap per `(currency, region)`, frozen in the snapshot and published on the read model. The rating boundary was already reserved and is unchanged: Tariffs sets amount/basis/attachment and emits `PeriodFloorCapObligation` (the **plan** ref is now defined rather than anticipated); **Billing executes** after step 9 (rating `fr-period-floor-cap-obligation`, §6.11/§17.2) — **no new evaluation machinery**. The **cap** landed with the floor rather than after it: the two share one `CHECK` (`floor ≤ cap`) which SQLite cannot add to an existing table, so deferring it would have cost a table rebuild to buy nothing. Distinct from committed-usage pools (Contracts SoR, rating T-D-14) — MUST NOT be conflated (rating §6.2). **What stays Future**: an authored *comparison basis* — whether a contractual floor claws back coupon discount is unresolved rating-side (rating §15), and the catalog does not author a value whose meaning is undecided |
| Two-dimensional pricing (seats x usage on one plan) | `p2` | Follow-on | Combines per-seat recurring (now in Scope) with a usage meter on the same line; multiple-meter math in Tariffs. **Decision gate** (owner Product): if any launch SKU needs single-line seats x usage it re-scopes out of Future (workaround = two rows) |
| Derived (composite) meter pricing (e.g. VM = vCPU + RAM) | `p2` | Follow-on | **Primitive defined** (§17.7); composes published units from the registry; unblocks Tariffs composite-meter (Tariffs Follow-on) |
| Dimensional pricing (event properties) | `p2` | Deferred | OSS/Rating dimension contract |
| Authorable `aggregationFunction` (`sum` \| `peak` \| `time_weighted`) / level-based billing | `p1` | **Decided → Scope (2026-07-16, D-44)** | Re-scoped into **launch** (supersedes 2026-07-04 "not at launch"): granule-fold (`aggregationGranularity {hour, day}`) summed into additive `Q`, frozen in `pricingSnapshotRef`; joint Rating fixture required (rating T-D-17); `last`/`unique` stay Future; no co-occurrence with composite meters at launch |
| Self-service reserved-capacity rate (reserved/on-demand pair) | — | **Decided -> Scope (2026-07-04)** | Now in **launch Scope** (§17.7); Tariffs sources the reserved rate from the catalog snapshot; negotiated RI-style rates stay in Contracts |
| Percentage-of-base pricing model kind | `p2` | Follow-on | Tariffs Future scope |
| Structural free-tier / freemium flag (beyond per-row $0) | `p2` | Follow-on | Per-row `$0` is **now publishable**; a first-class freemium plan flag remains Future. **Decision gate** (owner Product + Analytics): if reporting cannot distinguish a freemium plan from a `$0` promo band, the flag re-scopes into launch |
| First-class `includedAllowance` (+ `rolloverPolicy`) on usage rows | `p1` | **Decided → Scope (2026-07-16, D-45)** | Supersedes the 2026-07-04 keep-$0-band: authored first-class, publish-compiled (`none` → $0 band + marker; `carry` → D-43 grant); rollover **execution** stays downstream (Billing); `sum` rows only; per-seat / level-meter allowance = named Future gates |
| Self-service term / auto-renew metadata on Plan | `p2` | Cross-PRD | **Decided: not at launch (2026-07-04)** — explicit boundary (Out of Scope). If later needed, add optional `termLength`/`autoRenew` frozen in snapshot |
| Customer-group **different-tier-structure** per group | `p2` | Cross-PRD | **Decided (2026-07-04, F-88): the adjustment-based segment overlay is now in launch Scope** (§17.7). What stays Future: a group needing an **entirely different tier structure** — that requires separate plans + group-scoped plan eligibility |
| Typed credit/discount price rows (negative amounts) | `p2` | Follow-on | Negative amounts allowed only on explicitly typed credit rows; today rejected |
| Interim catalog `discountRef` reference field | — | **Decided -> Scope (2026-07-04)** | Committed as the **day-1 discount hook** (referential-integrity only — no authoring/evaluation here). A full Promotions PRD remains the durable owner |
| Minimal in-catalog list-discount price row | `p2` | Conditional | Last-resort fallback if both Promotions PRD and `discountRef` are insufficient at launch: a typed list-discount row (referential-integrity only, no stacking/evaluation here). Decision tied to the day-1 discount program gate |
| Explicit `currencyFallbackPolicy` (FX fallback) | `p3` | Deferred | Default is fail-closed (no implicit FX); fallback requires a configured policy |
| Catalog `refundable` / `creditPolicy` per price row | `p3` | Cross-PRD | Refund execution owned by Billing/Payments; catalog flag only if Finance requires it. **Decision gate** (owner Finance + Billing): if cancellation credits are required at launch, the per-row `cancellationCreditBasis` (conditional AC #86, fail-safe = no credit) re-scopes into launch |
| Marketplace listing eligibility rules per plan | `p3` | Follow-on | Tied to manifest §4.8 |

---

*Child artifacts: ADR(s) for canonical-scope-key and snapshot-versioning strategy; DESIGN for the catalog read model, publish/validation pipeline, and Tariffs / Subscriptions / Registry / Billing integration contracts.*
